//! The engine's frames (design §5.6): each frame's [`Header`], php's
//! `zend_execute_data`, laid in the room the VM stack keeps before the
//! frame's slots, so a frame is one record on the stack, pushed and
//! popped whole; what only some frames own beside it, in an [`Annex`].
//! A call is the same record from its `INIT_*`, which pushes it with the
//! callee's room, through the `SEND_*` oplines, which write the callee's
//! slots, to the `DO_*CALL` that activates it in place or serves the
//! native over it, as php's `zend_execute_data` is one from
//! `zend_vm_stack_push_call_frame` to `zend_vm_stack_free_call_frame`.
//!
//! The slots are the heap's: a CV or a temporary is a zval of a
//! [`HeapFrame`], read and written through the request's `slot`,
//! `take_slot` and `set_slot` as a [`Value`] naming what the zval names
//! with no owner taken (design §5.1), and the engine takes and gives up
//! owners here where php's handlers do, with `ZVAL_COPY`,
//! `ZVAL_COPY_VALUE` and `FREE_OP` as [`take_operand`], [`operand`] and
//! [`free_operand`].

use std::cell::Cell;

use php_heap::{Fatal, FrameRef, Zval};
use php_unit::decl::{Bare, HookKind};
use php_unit::{CacheSlot, ConstIdx, Level, OpArray, OperandKind, Opline, PublishedUnit, Unit};

use crate::api::call::Function;
use crate::api::raising::{Thrown, warning};
use crate::api::request::Request;
use crate::api::request::ropes::RopeAt;
use crate::api::values::{ClassId, Owner, PhpObject, PhpString, TypeTag, Value};

mod caches;

pub(crate) use caches::{CacheAt, RuntimeCaches, op_arrays_mut};

/// The VM stack a request or a fiber runs on, its frames' headers the
/// engine's ([`php_heap::VmStack`]).
pub(crate) type Stack = php_heap::VmStack<Header>;

/// One frame of a [`Stack`], or one a generator holds off it.
pub(crate) type HeapFrame = php_heap::Frame<Header>;

/// php's `execute_fake`: a room a generator holds, which a resumption
/// through it links between the frame it runs and the resumer's
/// ([`Header::fake`]).
pub(crate) type FakeRoom = php_heap::FakeRoom<Header>;

/// Which op array of a unit a frame serves: the main one, the body of
/// one of the unit's functions, the body of a function no name bound
/// while the file compiled and a `DECLARE_FUNCTION` bound where it
/// stands ([`Unit::dynamic`]), the body of one of its classes'
/// methods ([`Unit::methods`]), or of one of their properties' hooks
/// ([`Unit::hooks`]), each by its place in the table that holds it, as
/// `EX(func)` points at one `zend_op_array`: reading the op array is
/// one index, never a search. A method's or a hook's place is found
/// once, where its class binds it ([`Body::method`], [`Body::hook`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Body {
    Main,
    Function(u32),
    Dynamic(u32),
    Method(u32),
    Hook(u32),
}

impl Body {
    /// The body of method `method` of class `class` of `unit`, by its
    /// place among the unit's method bodies; none where the class
    /// declares no body for it.
    pub(crate) fn method(unit: &Unit, class: u32, method: u32) -> Option<Body> {
        let at = unit
            .methods
            .iter()
            .position(|m| m.class == class && m.method == method)?;
        u32::try_from(at).ok().map(Body::Method)
    }

    /// The body of the `kind` hook of property `property` of class
    /// `class` of `unit`, by its place among the unit's hook bodies.
    pub(crate) fn hook(unit: &Unit, class: u32, property: u32, kind: HookKind) -> Option<Body> {
        let at = unit
            .hooks
            .iter()
            .position(|h| h.class == class && h.property == property && h.kind == kind)?;
        u32::try_from(at).ok().map(Body::Hook)
    }
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

    /// The keyword as php's known string, `zend_fetch_debug_backtrace`'s
    /// `pseudo_function_name`.
    pub(crate) fn known(self) -> crate::engine::Known {
        use crate::engine::Known;
        match self {
            IncludeKind::Eval => Known::Eval,
            IncludeKind::Include => Known::Include,
            IncludeKind::IncludeOnce => Known::IncludeOnce,
            IncludeKind::Require => Known::Require,
            IncludeKind::RequireOnce => Known::RequireOnce,
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
    pub(crate) unit: PublishedUnit,
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

/// A [`Caller`] in the header's one half-word: none for
/// [`Caller::Outside`], and otherwise the frame one more than its place,
/// as [`AnnexId`] is, with [`CallerAt::LOOP`] set for [`Caller::Loop`]. A
/// stack's frames are numbered far below the bit, each taking a room of
/// eighty bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CallerAt(Option<std::num::NonZeroU32>);

impl CallerAt {
    const LOOP: u32 = 1 << 31;

    fn of(caller: Caller) -> CallerAt {
        let (frame, from_loop) = match caller {
            Caller::Opline(frame) => (frame, false),
            Caller::Loop(frame) => (frame, true),
            Caller::Outside => return CallerAt(None),
        };
        let bit = if from_loop { CallerAt::LOOP } else { 0 };
        CallerAt(Some(
            std::num::NonZeroU32::MIN.saturating_add(frame.0 & !CallerAt::LOOP) | bit,
        ))
    }

    fn caller(self) -> Caller {
        let Some(packed) = self.0 else {
            return Caller::Outside;
        };
        let frame = FrameRef((packed.get() & !CallerAt::LOOP).saturating_sub(1));
        if packed.get() & CallerAt::LOOP == 0 {
            Caller::Opline(frame)
        } else {
            Caller::Loop(frame)
        }
    }
}

/// The slot of the caller's that takes what a frame returns, one more
/// than the slot so the header's `Option` of it is a half-word, as
/// [`AnnexId`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReturnSlot(std::num::NonZeroU32);

impl ReturnSlot {
    fn of(slot: u32) -> ReturnSlot {
        ReturnSlot(std::num::NonZeroU32::MIN.saturating_add(slot))
    }

    fn slot(self) -> u32 {
        self.0.get() - 1
    }
}

/// The called scope in the header's half-word, its class's identity
/// inverted so the `Option` costs nothing: [`ClassId::local`] numbers no
/// class at `u32::MAX`, so every class has a non-zero inversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScopeAt(std::num::NonZeroU32);

impl ScopeAt {
    fn of(scope: Option<ClassId>) -> Option<ScopeAt> {
        std::num::NonZeroU32::new(!scope?.0).map(ScopeAt)
    }

    fn class(self) -> ClassId {
        ClassId(!self.0.get())
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
/// here its destructor, its members' release and its id are served at
/// that moment while its bytes stand until the last pin is let go
/// ([`Request::give_up_receiver`]); and a frame that leaves the
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

/// What php's `INCLUDE_OR_EVAL`, and `zend_execute_scripts` for the
/// primary file and the files `auto_prepend_file` and `auto_append_file`
/// name, keep on the request's heap for the op array a code frame
/// serves, freed with the frame however it ends
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
/// throw it meets, peaks at php's sizes. The rows read as the slice
/// they are; [`ExtraNamed::rows_mut`] takes one more and
/// [`ExtraNamed::lay`] grows the table to hold them. A call or a frame
/// with none, which is nearly every one, holds one empty word, as php's
/// `ZEND_CALL_HAS_EXTRA_NAMED_PARAMS` bit is clear and the pointer
/// unread, so pushing and popping it moves and frees nothing.
#[derive(Default)]
pub(crate) struct ExtraNamed(Option<Box<Named>>);

/// The rows and the table of an [`ExtraNamed`] that holds any.
#[derive(Default)]
struct Named {
    rows: Vec<(PhpString, Value)>,
    table: Option<(php_heap::Block, php_heap::Block, usize)>,
}

impl ExtraNamed {
    /// `rows` with the table laid to hold them ([`ExtraNamed::lay`]).
    pub(crate) fn laid(
        req: &mut Request,
        rows: Vec<(PhpString, Value)>,
    ) -> Result<ExtraNamed, Thrown> {
        let mut named = ExtraNamed(Some(Box::new(Named { rows, table: None })));
        named.lay(req)?;
        Ok(named)
    }

    /// The table on the request's heap grown to hold every row:
    /// `zend_new_array(0)` and `zend_hash_real_init_mixed` at the first
    /// row, `zend_hash_do_resize` each time the rows pass its size.
    pub(crate) fn lay(&mut self, req: &mut Request) -> Result<(), Thrown> {
        let Some(named) = self.0.as_deref_mut().filter(|named| !named.rows.is_empty()) else {
            return Ok(());
        };
        let (head, mut data, mut size) = if let Some(table) = named.table.take() {
            table
        } else {
            let head = req.heap_mut().block(56);
            let head = req.fatal_of(head)?;
            let data = req.heap_mut().block(8 * 40);
            (head, req.fatal_of(data)?, 8)
        };
        while named.rows.len() > size {
            size <<= 1;
            let grown = req.heap_mut().block(size * 40);
            let grown = req.fatal_of(grown)?;
            req.heap_mut()
                .free_block(std::mem::replace(&mut data, grown));
        }
        named.table = Some((head, data, size));
        Ok(())
    }

    /// The table alone, its rows left here: what a frame hands on where
    /// the rows move into a variadic's array and php's table stands until
    /// the frame is freed.
    pub(crate) fn take_table(&mut self) -> ExtraNamed {
        let table = self.0.as_deref_mut().and_then(|named| named.table.take());
        ExtraNamed(table.map(|table| {
            Box::new(Named {
                rows: Vec::new(),
                table: Some(table),
            })
        }))
    }

    /// The rows, to be given up by the caller, the table freed.
    pub(crate) fn free(self, req: &mut Request) -> Vec<(PhpString, Value)> {
        let Some(named) = self.0 else {
            return Vec::new();
        };
        let Named { rows, table } = *named;
        if let Some((head, data, _)) = table {
            req.heap_mut().free_block(data);
            req.heap_mut().free_block(head);
        }
        rows
    }

    /// Whether a row or a table stands, which is
    /// `ZEND_CALL_HAS_EXTRA_NAMED_PARAMS`.
    pub(crate) fn carries_any(&self) -> bool {
        self.0.is_some()
    }

    /// The rows moved out, the table left where it stands.
    pub(crate) fn take_rows(&mut self) -> Vec<(PhpString, Value)> {
        self.0
            .as_deref_mut()
            .map(|named| std::mem::take(&mut named.rows))
            .unwrap_or_default()
    }

    /// The rows to add one to.
    pub(crate) fn rows_mut(&mut self) -> &mut Vec<(PhpString, Value)> {
        &mut self.0.get_or_insert_with(Box::default).rows
    }
}

impl std::ops::Deref for ExtraNamed {
    type Target = [(PhpString, Value)];

    fn deref(&self) -> &Self::Target {
        self.0.as_deref().map_or(&[], |named| named.rows.as_slice())
    }
}

impl std::ops::DerefMut for ExtraNamed {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
            .as_deref_mut()
            .map_or(&mut [], |named| named.rows.as_mut_slice())
    }
}

/// A frame's header, php's `zend_execute_data`, laid in the room the VM
/// stack keeps before the frame's slots, past the stack's link (design
/// §5.6): one record with the slots after it, pushed and popped with
/// them, written whole by the push and read and written in place after.
/// Everything here is a word or a half-word; what a frame owns and only
/// some frames have stands in its [`Annex`], made where it is needed and
/// named by [`Header::annex`]. It is `#[repr(C)]` because another
/// process reads three of its fields at fixed offsets, the record,
/// the opline and the flags ([`crate::readable`]), and the fields are in
/// that order and then by size, so the header fills the seventy-two
/// bytes php's eighty leave past the link with nothing between them.
#[derive(Debug, Clone)]
#[repr(C)]
pub(crate) struct Header {
    /// `EX(func)` as another process reads it: the record of what
    /// the frame serves ([`crate::readable::Record`]), written by the
    /// push and never read here.
    pub(crate) callee: crate::readable::CalleeAt,
    /// `Z_OBJ(EX(This))`: the receiver of a method call; whether the
    /// frame owns it is [`CallInfo::release_this`] ([`Receiver`]).
    this: Option<PhpObject>,
    /// The `Closure` object whose call pushed the frame, one owner the
    /// frame holds until it is popped (`ZEND_CALL_CLOSURE`,
    /// `zend_vm_stack_free_call_frame`'s `OBJ_RELEASE`): what php
    /// reaches through `EX(func)->op_array.static_variables_ptr`, since
    /// the op array a closure serves is the closure's own copy. The
    /// `BIND_STATIC` prologue of the body reads its bound variables off
    /// this and nothing else does; none for every other frame.
    pub(crate) closure: Option<PhpObject>,
    /// The frame's identity, given the first time an extension keeps it
    /// ([`crate::api::request::FrameId`]) and never given again in the
    /// request; none until then. It moves with the header, so a
    /// generator's frame keeps it across its resumptions as php's
    /// `execute_data` is one, and a frame pushed later where this one
    /// stood has none or another.
    pub(crate) identity: Cell<Option<crate::api::request::Identity>>,
    /// Where in the op array the loop stands, `EX(opline)`.
    pub(crate) opline: u32,
    /// `ZEND_CALL_INFO`: the frame's flags.
    pub(crate) info: CallInfo,
    /// `EX(func)`: the linked function the frame serves; none for an
    /// included, evaluated or main file's op array, which is no
    /// function and whose unit the frame's [`Annex`] holds.
    pub(crate) function: Option<Function>,
    /// `ZEND_CALL_NUM_ARGS`: how many arguments the call that pushed the
    /// frame placed, which the `RECV*` prologue reads.
    pub(crate) num_args: u32,
    /// `EX(prev_execute_data)` as far as a diagnostic reads it
    /// ([`Caller`], [`Header::caller`]).
    caller: CallerAt,
    /// The slot of the caller's ([`Caller::frame`]) that takes what this
    /// frame returns, when the call that pushed it wants the value
    /// ([`Header::returns_to`]).
    returns_to: Option<ReturnSlot>,
    /// The called scope, `static`; none for the main op array or a
    /// function ([`Header::called_scope`]).
    called_scope: Option<ScopeAt>,
    /// `EX(run_time_cache)`: where the op array's runtime cache stands
    /// in the request's map table (design §5.6), read off the op array
    /// as `RUN_TIME_CACHE(op_array)` reads it when the frame is
    /// activated. The frame holds the pointer and never the cache: the
    /// cache is the request's, laid down on the op array's first
    /// activation ([`init_runtime_cache`]) and kept until request end, so
    /// a function called twice reads on its second call what its first
    /// primed.
    run_time_cache: CacheAt,
    /// What the frame owns beyond words, where it owns anything.
    pub(crate) annex: Option<AnnexId>,
}

// The header fills the room php's `zend_execute_data` leaves past the
// stack's link, and no more (`php_heap::Frame`'s `FITS` holds the same).
const _: () = assert!(
    std::mem::size_of::<Header>() == php_heap::ROOM_BYTES - php_heap::HEADER_AT,
    "a frame header fills php's zend_execute_data room past the link"
);

impl Header {
    /// The header of a frame serving `function`, or a file's op array
    /// where it is none, whose runtime cache is `code`'s, at its first
    /// opline, with no receiver, closure, scope or argument, returning
    /// to no slot, pushed from outside the loop.
    pub(crate) fn of(function: Option<Function>, code: &OpArray) -> Header {
        Header {
            callee: crate::readable::CalleeAt::default(),
            function,
            this: None,
            closure: None,
            called_scope: None,
            caller: CallerAt(None),
            returns_to: None,
            identity: Cell::new(None),
            opline: 0,
            num_args: 0,
            run_time_cache: CacheAt::of(code.runtime_cache),
            annex: None,
            info: CallInfo::default(),
        }
    }

    /// The header of php's `execute_fake`, a `zend_execute_data` that
    /// serves nothing, its `func` `NULL`: on php's chain where it is
    /// linked, and named by no record, so another process reads it as
    /// `???` (`Z/sapi/fpm/fpm/fpm_php_trace.c`).
    pub(crate) fn fake() -> Header {
        Header {
            callee: crate::readable::CalleeAt::default(),
            function: None,
            this: None,
            closure: None,
            called_scope: None,
            caller: CallerAt(None),
            returns_to: None,
            identity: Cell::new(None),
            opline: 0,
            num_args: 0,
            run_time_cache: CacheAt::default(),
            annex: None,
            info: CallInfo::default(),
        }
    }

    /// The header `zend_vm_stack_push_call_frame` writes for a call to
    /// `function` the `INIT_*` says `count` arguments are passed to:
    /// being built ([`CallInfo::building`]), no receiver, no scope.
    pub(crate) fn call(function: Function, count: u32) -> Header {
        let mut info = CallInfo::default();
        info.set(CallInfo::CALL | CallInfo::BUILDING, true);
        Header {
            callee: crate::readable::CalleeAt::default(),
            function: Some(function),
            this: None,
            closure: None,
            called_scope: None,
            caller: CallerAt(None),
            returns_to: None,
            identity: Cell::new(None),
            opline: 0,
            num_args: count,
            run_time_cache: CacheAt::default(),
            annex: None,
            info,
        }
    }

    /// A call made a user frame over `code`, `i_init_func_execute_data`:
    /// the opline it stands at, the runtime cache it reads, and whether
    /// it was passed arguments past its parameters.
    pub(crate) fn activate(&mut self, run_time_cache: CacheAt, opline: u32, extra_args: bool) {
        self.run_time_cache = run_time_cache;
        self.opline = opline;
        self.info.set(CallInfo::CALL | CallInfo::BUILDING, false);
        self.info.set(CallInfo::EXTRA_ARGS, extra_args);
    }

    /// `EX(This)` as a receiver, with whether the frame owns it.
    pub(crate) fn receiver(&self) -> Option<Receiver> {
        self.this.map(|this| {
            if self.info.release_this() {
                Receiver::Owned(this)
            } else {
                Receiver::Lent(this)
            }
        })
    }

    /// `EX(This)` and `ZEND_CALL_RELEASE_THIS` written from `receiver`.
    pub(crate) fn set_receiver(&mut self, receiver: Option<Receiver>) {
        self.this = receiver.map(Receiver::object);
        self.info
            .set_release_this(matches!(receiver, Some(Receiver::Owned(_))));
    }

    /// `Z_OBJ(EX(This))`: the receiver, a view.
    pub(crate) fn this(&self) -> Option<PhpObject> {
        self.this
    }

    /// `EX(run_time_cache)`.
    pub(crate) fn runtime_cache(&self) -> CacheAt {
        self.run_time_cache
    }

    /// `EX(prev_execute_data)` as far as a diagnostic reads it.
    pub(crate) fn caller(&self) -> Caller {
        self.caller.caller()
    }

    pub(crate) fn set_caller(&mut self, caller: Caller) {
        self.caller = CallerAt::of(caller);
    }

    /// The slot of the caller's that takes what the frame returns.
    pub(crate) fn returns_to(&self) -> Option<u32> {
        self.returns_to.map(ReturnSlot::slot)
    }

    pub(crate) fn set_returns_to(&mut self, slot: Option<u32>) {
        self.returns_to = slot.map(ReturnSlot::of);
    }

    /// The called scope, `static`.
    pub(crate) fn called_scope(&self) -> Option<ClassId> {
        self.called_scope.map(ScopeAt::class)
    }

    pub(crate) fn set_called_scope(&mut self, scope: Option<ClassId>) {
        self.called_scope = ScopeAt::of(scope);
    }

    /// Where another process reads the record, the opline and the
    /// flags, past the room's link ([`crate::readable::Room::read`]).
    pub(crate) const CALLEE_AT: usize = std::mem::offset_of!(Header, callee);
    pub(crate) const OPLINE_AT: usize = std::mem::offset_of!(Header, opline);
    pub(crate) const INFO_AT: usize = std::mem::offset_of!(Header, info);
}

/// `ZEND_CALL_INFO`'s flags as this engine's frame has them, one bit
/// each behind a name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct CallInfo(u32);

impl CallInfo {
    /// `ZEND_CALL_RELEASE_THIS`: the frame owns its receiver.
    const RELEASE_THIS: u32 = 1 << 0;
    /// The frame is being popped: `zend_leave_helper` has written
    /// `EG(current_execute_data) = EX(prev_execute_data)` ahead of
    /// `i_free_compiled_variables`, so a destructor what it gives up
    /// serves is called from the caller's activation, and this one is
    /// on no chain a backtrace reads.
    const LEAVING: u32 = 1 << 1;
    /// `EX(opline) == EG(exception_op)`: the unwinder is answering a
    /// throwable in the frame (`ZEND_HANDLE_EXCEPTION`), from the throw
    /// until it lands at a `catch` or a `finally`, or ends the frame at
    /// its uncaught arm. `opline` keeps the opline that raised, php's
    /// `EG(opline_before_exception)`, which a trace reads; this is what
    /// an observer that reads the opcode reads.
    const AT_EXCEPTION_OP: u32 = 1 << 2;
    /// The record is a call and not a user frame: one an `INIT_*`
    /// pushed that no `DO_*CALL` has made a frame of, or a native's
    /// being served over its arguments. A backtrace reads the second as
    /// php's internal frame and passes the first, as
    /// `zend_fetch_debug_backtrace` walks `EG(current_execute_data)`, a
    /// chain a frame `INIT_FCALL` has pushed and nothing has activated
    /// is not on.
    const CALL: u32 = 1 << 3;
    /// The call is being built, its `SEND_*` oplines placing arguments.
    const BUILDING: u32 = 1 << 4;
    /// `!USED_RET()` (`Z/Zend/zend_API.h`): the `DO_*CALL` serving the
    /// call writes its `result` to an `UNUSED` operand. A call
    /// `zend_call_function` makes reads it, as php's test finds the
    /// frame before the callee's internal.
    const RETURN_UNUSED: u32 = 1 << 5;
    /// `ZEND_CALL_DYNAMIC`: the callee was named as the program served,
    /// by `INIT_DYNAMIC_CALL` or `INIT_USER_CALL`, which a native that
    /// reads its caller's frame refuses (`zend_forbid_dynamic_call`).
    const DYNAMIC: u32 = 1 << 6;
    /// `ZEND_CALL_SEND_ARG_BY_REF`: what the last `CHECK_FUNC_ARG` read
    /// off the callee for the argument after it, whether its parameter
    /// takes it by reference, which the `_FUNC_ARG` fetch and the
    /// `SEND_FUNC_ARG` after it serve as a write or as a read by.
    const SEND_BY_REF: u32 = 1 << 7;
    /// A `FRAMELESS_ICALL_*`'s call, whose arguments are the opline's
    /// operands: a coercion of one is kept in the [`Annex`] and not
    /// written over the argument, as `Z_FLF_PARAM_STR` converts in a
    /// temporary of the body's and a trace still reads the operand.
    const FRAMELESS: u32 = 1 << 8;
    /// `ZEND_CALL_FREE_EXTRA_ARGS`: the activation moved arguments past
    /// the declared parameters beyond the CVs and the temporaries
    /// (`zend_copy_extra_args`), which the frame's leave gives up.
    const EXTRA_ARGS: u32 = 1 << 9;
    /// `ZEND_CALL_TOP_CODE`: the frame of a main op array, pushed with
    /// nothing executing below it, where the walk another process makes
    /// of the chain ends (`fpm_php_trace_dump`).
    const TOP_CODE: u32 = 1 << 10;
    /// `quick_arg_flags` (`ZEND_CHECK_ARG_FLAG`, `Z/Zend/zend_compile.h`)
    /// as the `INIT` read it off a user callee
    /// ([`crate::api::request::FrameShape::sent`]): the bit this many
    /// places up, and `n - 1` more, set where the argument numbered `n`,
    /// up to [`CallInfo::QUICK`], goes by value for certain, its
    /// parameter, or past the declared ones the variadic, taking none by
    /// reference. Clear where the declaration is asked, as it is for a
    /// call whose `INIT` wrote nothing, a native's.
    const BY_VALUE_AT: u32 = 11;
    /// `MAX_ARG_FLAG_NUM`: how many arguments [`CallInfo::BY_VALUE_AT`]
    /// covers.
    const QUICK: u32 = 12;
    /// The `INIT` found the callee a user function that raises neither
    /// a deprecation nor a discarded value's warning
    /// ([`crate::api::request::FrameShape::sent`]), so its activation
    /// asks neither ([`crate::api::request::Request::enter_call`]).
    const PLAIN_ENTRY: u32 = 1 << 23;
    /// The call is a closure's that reads a runtime cache of its own
    /// (`ZEND_ACC_HEAP_RT_CACHE`), which its activation reads in place
    /// of its function's: a [`CallInfo::PLAIN_ENTRY`] it overrides.
    const OWN_CACHE: u32 = 1 << 24;

    /// The flags as another process read them ([`crate::readable`]).
    pub(crate) fn of(bits: u32) -> CallInfo {
        CallInfo(bits)
    }

    pub(crate) fn top_code(self) -> bool {
        self.has(Self::TOP_CODE)
    }

    pub(crate) fn set_top_code(&mut self, on: bool) {
        self.set(Self::TOP_CODE, on);
    }

    fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }

    fn set(&mut self, bit: u32, on: bool) {
        if on {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }

    pub(crate) fn release_this(self) -> bool {
        self.has(Self::RELEASE_THIS)
    }

    pub(crate) fn set_release_this(&mut self, on: bool) {
        self.set(Self::RELEASE_THIS, on);
    }

    pub(crate) fn leaving(self) -> bool {
        self.has(Self::LEAVING)
    }

    pub(crate) fn set_leaving(&mut self, on: bool) {
        self.set(Self::LEAVING, on);
    }

    pub(crate) fn at_exception_op(self) -> bool {
        self.has(Self::AT_EXCEPTION_OP)
    }

    pub(crate) fn set_at_exception_op(&mut self, on: bool) {
        self.set(Self::AT_EXCEPTION_OP, on);
    }

    /// The frame holds arguments past its declared parameters.
    pub(crate) fn extra_args(self) -> bool {
        self.has(Self::EXTRA_ARGS)
    }

    /// The record is a call, being built or a native's being served.
    pub(crate) fn call(self) -> bool {
        self.has(Self::CALL)
    }

    /// The call is being built.
    pub(crate) fn building(self) -> bool {
        self.has(Self::BUILDING)
    }

    /// A native's call being served.
    pub(crate) fn serving_native(self) -> bool {
        self.has(Self::CALL) && !self.has(Self::BUILDING)
    }

    /// A user frame: a function's or a file's, activated.
    pub(crate) fn user(self) -> bool {
        !self.has(Self::CALL)
    }

    /// `DO_*CALL` serving the call: no longer being built.
    pub(crate) fn set_serving(&mut self) {
        self.set(Self::BUILDING, false);
    }

    pub(crate) fn set_building(&mut self, on: bool) {
        self.set(Self::BUILDING, on);
    }

    pub(crate) fn return_used(self) -> bool {
        !self.has(Self::RETURN_UNUSED)
    }

    pub(crate) fn set_return_used(&mut self, used: bool) {
        self.set(Self::RETURN_UNUSED, !used);
    }

    pub(crate) fn dynamic(self) -> bool {
        self.has(Self::DYNAMIC)
    }

    pub(crate) fn set_dynamic(&mut self, on: bool) {
        self.set(Self::DYNAMIC, on);
    }

    pub(crate) fn send_by_ref(self) -> bool {
        self.has(Self::SEND_BY_REF)
    }

    pub(crate) fn set_send_by_ref(&mut self, on: bool) {
        self.set(Self::SEND_BY_REF, on);
    }

    pub(crate) fn frameless(self) -> bool {
        self.has(Self::FRAMELESS)
    }

    /// The by-value bits of a function whose argument numbered `n` is
    /// taken by reference where `by_ref(n)` says so
    /// ([`CallInfo::BY_VALUE_AT`]), what its `INIT` writes.
    pub(crate) fn sent(by_ref: impl Fn(u32) -> bool) -> CallInfo {
        let bits = (1..=Self::QUICK)
            .filter(|&number| !by_ref(number))
            .fold(0, |bits, number| {
                bits | 1 << (Self::BY_VALUE_AT + number - 1)
            });
        CallInfo(bits)
    }

    /// `!QUICK_ARG_SHOULD_BE_SENT_BY_REF(EX(call)->func, number)` where
    /// the `INIT` wrote it: the argument numbered `number` goes by value
    /// for certain. `false` past [`CallInfo::QUICK`] and where nothing
    /// was written, which asks the declaration.
    #[inline]
    pub(crate) fn sends_by_value(self, number: u32) -> bool {
        let at = number.wrapping_sub(1);
        at < Self::QUICK && self.0 & 1 << (Self::BY_VALUE_AT + at) != 0
    }

    /// [`CallInfo::sent`] with [`CallInfo::PLAIN_ENTRY`] where `plain`.
    pub(crate) fn with_plain_entry(self, plain: bool) -> CallInfo {
        CallInfo(if plain {
            self.0 | Self::PLAIN_ENTRY
        } else {
            self.0
        })
    }

    /// An activation that reads the function's own cache and raises
    /// nothing for its attributes: [`CallInfo::PLAIN_ENTRY`] and no
    /// [`CallInfo::OWN_CACHE`].
    #[inline]
    pub(crate) fn plain_entry(self) -> bool {
        self.0 & (Self::PLAIN_ENTRY | Self::OWN_CACHE) == Self::PLAIN_ENTRY
    }

    pub(crate) fn set_own_cache(&mut self, on: bool) {
        self.set(Self::OWN_CACHE, on);
    }

    /// The bits `more` sets set here too: what an `INIT` writes of its
    /// callee into the call's flags.
    #[inline]
    pub(crate) fn join(&mut self, more: CallInfo) {
        self.0 |= more.0;
    }

    pub(crate) fn set_frameless(&mut self, on: bool) {
        self.set(Self::FRAMELESS, on);
    }
}

/// What a frame owns beyond the words of its [`Header`], made only for a
/// frame that owns any of it: a file's frame (its unit, its scope, its
/// symbol table, the op array an include holds), a function's frame
/// that rebuilt a symbol table, took extra arguments or entered a
/// `finally`, and a generator's.
#[derive(Default)]
pub(crate) struct Annex {
    /// The unit whose main op array a file's frame serves, one share held
    /// for the frame's life: a function's frame has its unit through the
    /// function table, which holds it.
    pub(crate) unit: Option<PublishedUnit>,
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
    /// `ZEND_CALL_HAS_EXTRA_NAMED_PARAMS` and `EX(extra_named_params)`:
    /// the named arguments the function declares no parameter for,
    /// which `zend_handle_named_arg` collects for a variadic callee, in
    /// the order they were given; `RECV_VARIADIC` adds them to its
    /// array by name after the extra arguments, and a backtrace prints
    /// them `name: value`. Each name and value one owner of the frame's,
    /// given up with it (`zend_free_extra_named_params`).
    pub(crate) extra_named: ExtraNamed,
    /// The `Generator` whose frame this is, php's `EX(return_value)`
    /// under `ZEND_CALL_GENERATOR` (`zend_get_running_generator`): set
    /// by `GENERATOR_CREATE` when it moves the frame into the object,
    /// read by `YIELD`, `YIELD_FROM` and `GENERATOR_RETURN`. A view with
    /// no owner, since the object owns the frame and not the other way
    /// round; none for every other frame.
    pub(crate) generator: Option<PhpObject>,
    /// How many times the frame's table had been written when the frame
    /// last attached to it ([`Request::attach_symbol_table`]), every CV's
    /// bucket naming the CV from then on: an attach while the count
    /// stands moves nothing, as an includer's when the file it included
    /// wrote nothing to the table.
    pub(crate) attached: Option<u32>,
    /// php's fast-call temporaries, one per `finally` block of the op
    /// array a `FAST_CALL` or an unwind has entered. php keeps two
    /// fields in the one zval at `EX_VAR(opline->result.var)`,
    /// `Z_OPLINE_NUM_P` and `Z_OBJ_P`, which no zval of this engine
    /// holds together, so the cell stands here and the temporary itself
    /// is left `UNDEF`. Nothing but `FAST_CALL`, `FAST_RET`,
    /// `DISCARD_EXCEPTION` and the unwinder reads it.
    pub(crate) fast_calls: Vec<FastCall>,
    /// The op array's bytes an `INCLUDE_OR_EVAL` frame holds on the
    /// request's heap ([`HeapOpArray`]); none for every other frame.
    pub(crate) op_array: Option<HeapOpArray>,
    /// The name a parent hook's trampoline call was built under,
    /// `$name::get`, which php keeps in the trampoline's
    /// `internal_function.reserved[0]`; one owner the call holds.
    pub(crate) name: Option<PhpString>,
    /// php's `str_tmp`: what a coercion made for a frameless call, freed
    /// with the call as `Z_FLF_PARAM_FREE_STR` frees it at the body's end.
    pub(crate) coerced: Vec<Value>,
    /// How far down its native's chain of interceptions the call is
    /// served: zero for the entry the function table holds, the last
    /// interception's, and one more for each
    /// [`crate::api::call::passthrough`] to the entry that one replaced.
    pub(crate) beneath: usize,
    /// Where a file's frame finds its main op array's `static` table;
    /// none for a function's frame.
    pub(crate) statics: Option<FileStatics>,
}

/// php's `ZEND_MAP_PTR(static_variables_ptr)` of the main op array a
/// file's frame runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileStatics {
    /// The op array opcache loaded, whose map pointer every include of
    /// the file shares; `kept` where the frame's leave keeps the table,
    /// as `spl_autoload` serves a file with `zend_execute` and then
    /// `destroy_op_array` alone (`Z/ext/spl/php_spl.c:262`).
    Shared { kept: bool },
    /// An op array compiled afresh, whose table is its own, by the
    /// request's number for it.
    Own(u32),
}

impl Annex {
    /// The fast-call cell of the frame's temporary `var`, made where the
    /// frame has none yet: php's zval is the temporary itself and is
    /// always there, so a first read makes the cell php would have read.
    pub(crate) fn fast_call(&mut self, var: u32) -> Option<&mut FastCall> {
        if let Some(at) = self.fast_calls.iter().position(|cell| cell.var == var) {
            return self.fast_calls.get_mut(at);
        }
        self.fast_calls.push(FastCall {
            var,
            from: None,
            delayed: None,
        });
        self.fast_calls.last_mut()
    }
}

/// Where a frame's [`Annex`] stands among the request's, one more than
/// its place so a header's `Option` of it is one word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AnnexId(std::num::NonZeroU32);

/// Every frame's [`Annex`], the request's, so a frame keeps its own
/// across a move off one stack and onto another, a generator's resumed
/// on a fiber among them; a place given back is the next one taken.
/// Each place stands at one address from the first annex laid there to
/// the request's end, so a code frame's record is written where it
/// stands, as php's `EX(func)` is the op array the frame holds
/// ([`Annexes::callee`]), and laying an annex down where one stood
/// allocates nothing.
#[derive(Default)]
pub(crate) struct Annexes {
    // Boxed so a row keeps its address as the vector grows.
    #[allow(clippy::vec_box)]
    rows: Vec<Box<AnnexRow>>,
    free: Vec<u32>,
}

/// One place among the [`Annexes`]: the annex laid there, and beside it
/// the record another process reads of a code frame's op array
/// ([`crate::readable`]), no name, the unit's file and oplines, written
/// as the annex of a frame serving a unit's main op array is laid down.
#[derive(Default)]
struct AnnexRow {
    annex: Option<Annex>,
    record: crate::readable::Record,
}

impl Annexes {
    /// `extra` given a place, and the record of the op array it serves
    /// written beside it where it serves a unit's main op array.
    pub(crate) fn insert(&mut self, extra: Annex) -> AnnexId {
        let record = extra.unit.as_deref().map(code_record);
        let at = if let Some(at) = self.free.pop() {
            at
        } else {
            self.rows.push(Box::default());
            u32::try_from(self.rows.len() - 1).unwrap_or(u32::MAX - 1)
        };
        if let Some(row) = self.rows.get_mut(at as usize) {
            row.annex = Some(extra);
            if let Some(record) = record {
                row.record = record;
            }
        }
        AnnexId(std::num::NonZeroU32::MIN.saturating_add(at))
    }

    /// Where the record of the code frame whose annex is `id` stands,
    /// what its header names as `EX(func)`: it stands there until the
    /// annex is taken, after the frame is popped. None for a frame that
    /// serves no unit's main op array.
    pub(crate) fn callee(&self, id: AnnexId) -> crate::readable::CalleeAt {
        self.rows
            .get(id.0.get() as usize - 1)
            .filter(|row| row.annex.as_ref().is_some_and(|annex| annex.unit.is_some()))
            .map(|row| row.record.at())
            .unwrap_or_default()
    }

    pub(crate) fn get(&self, id: AnnexId) -> Option<&Annex> {
        self.rows.get(id.0.get() as usize - 1)?.annex.as_ref()
    }

    pub(crate) fn get_mut(&mut self, id: AnnexId) -> Option<&mut Annex> {
        self.rows.get_mut(id.0.get() as usize - 1)?.annex.as_mut()
    }

    /// The extra at `id` taken out, its place given back.
    pub(crate) fn take(&mut self, id: AnnexId) -> Option<Annex> {
        let at = id.0.get() - 1;
        let taken = self.rows.get_mut(at as usize)?.annex.take();
        if taken.is_some() {
            self.free.push(at);
        }
        taken
    }

    /// How many annexes stand: what a test counts to see a frame's go.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.rows.len() - self.free.len()
    }
}

/// One frame read where it stands, on a stack or off one, with the
/// request it belongs to: its header, its slots, its [`Annex`] and the op
/// array it serves, found through the function table or the file's
/// unit, as `EX(func)` points at one `zend_op_array`.
#[derive(Clone, Copy)]
pub(crate) struct FrameAt<'a> {
    pub(crate) req: &'a Request,
    pub(crate) frame: &'a HeapFrame,
}

impl<'a> FrameAt<'a> {
    pub(crate) fn header(self) -> &'a Header {
        self.frame.header()
    }

    pub(crate) fn annex(self) -> Option<&'a Annex> {
        self.req.annexes().get(self.header().annex?)
    }

    pub(crate) fn function(self) -> Option<Function> {
        self.header().function
    }

    pub(crate) fn this(self) -> Option<PhpObject> {
        self.header().this()
    }

    /// The callee of a call: every call names one, from its `INIT_*` on.
    pub(crate) fn callee(self) -> Function {
        self.function().unwrap_or(Function::PASS)
    }

    pub(crate) fn receiver(self) -> Option<Receiver> {
        self.header().receiver()
    }

    pub(crate) fn called_scope(self) -> Option<ClassId> {
        self.header().called_scope()
    }

    pub(crate) fn closure(self) -> Option<PhpObject> {
        self.header().closure
    }

    pub(crate) fn caller(self) -> Caller {
        self.header().caller()
    }

    pub(crate) fn opline(self) -> u32 {
        self.header().opline
    }

    pub(crate) fn num_args(self) -> u32 {
        self.header().num_args
    }

    pub(crate) fn at_exception_op(self) -> bool {
        self.header().info.at_exception_op()
    }

    pub(crate) fn scope(self) -> Option<ClassId> {
        self.annex().and_then(|extra| extra.scope)
    }

    pub(crate) fn symbol_table(self) -> Option<TableId> {
        self.annex().and_then(|extra| extra.symbol_table)
    }

    pub(crate) fn generator(self) -> Option<PhpObject> {
        self.annex().and_then(|extra| extra.generator)
    }

    /// The arguments past the declared parameters, which the
    /// activation moved past the temporaries (`zend_copy_extra_args`)
    /// for `func_get_args` to read: views, the frame's owners.
    pub(crate) fn extra_args(self) -> Vec<Value> {
        let Some(code) = self.code() else {
            return Vec::new();
        };
        let declared = self
            .function()
            .and_then(|function| self.req.user_function(function))
            .map_or(0, |user| user.shape.parameters);
        let count = self.num_args().saturating_sub(declared) as usize;
        let from = code.num_cvs.saturating_add(code.num_tmps) as usize;
        self.frame
            .slots()
            .get(from..)
            .unwrap_or_default()
            .iter()
            .take(count)
            .map(|z| Value::of_zval(*z))
            .collect()
    }

    /// A call's arguments as they stand in its slots, `ZEND_CALL_ARG`
    /// one through `ZEND_CALL_NUM_ARGS`: views, the call's owners.
    pub(crate) fn arguments(self) -> Vec<Value> {
        self.frame
            .slots()
            .iter()
            .take(self.num_args() as usize)
            .map(|z| Value::of_zval(*z))
            .collect()
    }

    pub(crate) fn info(self) -> CallInfo {
        self.header().info
    }

    pub(crate) fn extra_named(self) -> &'a [(PhpString, Value)] {
        self.annex().map_or(&[], |extra| &extra.extra_named[..])
    }

    /// The unit and which of its op arrays the frame serves.
    pub(crate) fn unit(self) -> Option<(&'a PublishedUnit, Body)> {
        self.req.unit_of(self.header())
    }

    /// The op array the frame serves.
    pub(crate) fn code(self) -> Option<&'a OpArray> {
        let (unit, body) = self.unit()?;
        body_of(unit, body)
    }

    /// `ZEND_CALL_USES_STRICT_TYPES(call)` (`Z/Zend/zend_compile.h:716`):
    /// whether the op array the frame serves was compiled under
    /// `declare(strict_types=1)`, which the compiler sets on every op
    /// array of a file that declared it ([`OpArray::strict_types`]).
    /// False for a frame whose body the unit has not, which is this
    /// engine's error and never a program's.
    pub(crate) fn strict_types(self) -> bool {
        self.code().is_some_and(|code| code.strict_types)
    }

    /// The line of the opline the frame stands at, `EX(opline)->lineno`;
    /// zero for a frame past its op array.
    pub(crate) fn line(self) -> u32 {
        self.code()
            .and_then(|code| opline_at(code, self.opline()))
            .map_or(0, |op| op.lineno)
    }

    /// The file of the op array the frame serves.
    pub(crate) fn file(self) -> Option<&'a [u8]> {
        self.code().map(|code| code.filename.0.as_ref())
    }
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
///
/// php's bytes for the cache are on the request's heap at php's size,
/// `cache_size`: carved from the compiler's arena for a function's or a
/// real closure's shared cache, which nothing gives back before the
/// request ends ([`lay_runtime_cache`]), and `emalloc`ed for a closure's
/// own, given back with the closure ([`RuntimeCache::room`]).
#[derive(Debug, Default)]
pub(crate) struct RuntimeCache {
    slots: Option<Box<[Cached]>>,
    /// A closure's own cache's `emalloc(cache_size)`
    /// (`zend_create_closure_ex`, `Z/Zend/zend_closures.c:813`), taken as
    /// the closure is made and given back with it; none for a cache the
    /// arena holds.
    pub(crate) room: Option<php_heap::Block>,
    observer: crate::observer::Slot,
    /// For a closure's op array, the scope its cache was laid down for
    /// by the first closure made over it, which php writes onto the op
    /// array as `func->common.scope` (`zend_create_closure_ex`): a
    /// closure made in or bound to that scope shares the cache, any
    /// other takes one of its own ([`crate::classes::ClosureCache`]).
    pub(crate) laid_for: Option<ClassId>,
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
    #[inline]
    pub(crate) fn get(&self, slot: CacheSlot) -> Cached {
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

/// `ZEND_OBSERVER_DATA(function)` reached through the function rather
/// than a frame, as `zend_observer_add_begin_handler` and its kin reach
/// it: the observer slot of the runtime cache at `ptr`, the function's
/// own ([`FrameShape`](crate::api::request::FrameShape)), in the
/// request's map table, none where the cache was never laid down, php's
/// `RUN_TIME_CACHE` still `NULL`.
pub(crate) fn observer_slot_of(
    map: &mut RuntimeCaches,
    ptr: CacheAt,
) -> Option<&mut crate::observer::Slot> {
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
/// A trait's method in each class that uses it reads a cache of its
/// own, laid down at the map pointer its class's row was given
/// (`zend_add_trait_method`, `Z/Zend/zend_inheritance.c:2369`, whose
/// copy `function_add_ref` gives a fresh `run_time_cache`), since its
/// slots hold what the scope decides: the activation lays the cache the
/// function's [`FrameShape`](crate::api::request::FrameShape) names,
/// which for a copy is not its op array's. A closure bound to a scope
/// other than the one its op array's cache was laid down for reads one
/// of its own (`zend_create_closure`, `Z/Zend/zend_closures.c:794`;
/// [`crate::classes::closure_cache`]), which the activation reads off
/// the frame's closure in place of its function's. A function, a class
/// and a constant resolve the same in every scope; a method's callee
/// does not, so [`Cached::ClassAndCallee`] carries the scope it was
/// resolved from and answers no other.
///
/// The `Err` is the memory limit's fatal, where the arena had not the
/// room for a function's cache, and nothing is laid down.
pub(crate) fn init_runtime_cache(
    map: &mut RuntimeCaches,
    heap: &mut php_heap::Heap,
    code: &OpArray,
    body: Body,
) -> Result<(), Fatal> {
    let at = CacheAt::of(code.runtime_cache);
    if body == Body::Main {
        map.map(at).lay_down(code.cache_size);
        return Ok(());
    }
    lay_runtime_cache(map, heap, at, code.cache_size)
}

/// `zend_init_func_run_time_cache` over a function's cache pointer and
/// size ([`FrameShape`](crate::api::request::FrameShape)): laid down on
/// the function's first activation in the request, found laid down by
/// every one after, and its `cache_size` bytes carved from the
/// compiler's arena as `init_func_run_time_cache_i` carves them
/// (`Z/Zend/zend_execute.c:4455`). The arena's blocks are 64 KiB, so a
/// cache moves nothing until the newest block has not the room for it,
/// and the call whose cache it is takes another block of the same size,
/// which holds 8189 caches of one slot, its header aside. Where the
/// first block stands full is the compile's, which carves a function's
/// op array from the same arena under php, so the witness reads the
/// second:
///
/// ```php
/// $f = __DIR__ . '/carved.php'; $src = "<?php function g() {}\n";
/// for ($i = 0; $i < 20000; $i++) { $src .= "function f$i() { g(); }\n"; }
/// file_put_contents($f, $src); include $f; unlink($f);
/// $m = memory_get_usage(); $first = $second = $grew = 0;
/// for ($i = 0; $i < 20000 && !$second; $i++) { ("f$i")(); $now = memory_get_usage(); if ($now !== $m) { if ($first) { $second = $i; $grew = $now - $m; } else { $first = $i; } } $m = $now; }
/// echo $second - $first, " ", $grew, "\n";
/// ```
/// ```expect
/// 8189 65536
/// ```
///
/// The `Err` is the memory limit's fatal, with nothing laid down.
#[inline]
pub(crate) fn lay_runtime_cache(
    map: &mut RuntimeCaches,
    heap: &mut php_heap::Heap,
    at: CacheAt,
    cache_size: u32,
) -> Result<(), Fatal> {
    if !map.laid(at) {
        heap.carve(cache_size as usize)?;
        map.map(at).lay_down(cache_size);
    }
    Ok(())
}

/// A closure's own cache laid down at its first call: its slots, the
/// room php's `emalloc` takes having been taken as the closure was made
/// ([`RuntimeCache::room`]).
pub(crate) fn lay_own_cache(map: &mut RuntimeCaches, at: CacheAt, cache_size: u32) {
    if !map.laid(at) {
        map.map(at).lay_down(cache_size);
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
    /// The constant a `FETCH_CONSTANT` found on its first pass, where it
    /// is not deprecated (`zend_quick_get_constant`'s `CACHE_PTR`).
    Constant(crate::api::request::ConstantAt),
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
    /// (`zend_create_closure`, `Z/Zend/zend_closures.c:794`). Both read a
    /// cache of their own here too ([`init_runtime_cache`]), and the pair
    /// is still read back only in the scope that wrote it. What the slot
    /// holds is what was asked and what answered, never how the answer
    /// was found.
    ///
    /// `from` is the function of the frame that wrote the pair where
    /// that frame is no closure's, which fixes its scope, as
    /// [`Cached::Property`]'s `function` does: a frame of that function
    /// reads the pair back without its scope being asked. And `instance`
    /// is whether the callee takes `$this`, the test of
    /// `fbc->common.fn_flags` php makes, read where the pair is written,
    /// as php reads it off the callee it holds.
    ClassAndCallee {
        class: ClassId,
        scope: Option<ClassId>,
        from: Option<Function>,
        function: Function,
        instance: bool,
    },
    /// The place a property fetch or assignment over a constant name
    /// found on its first pass, `CACHE_POLYMORPHIC_PTR_EX(cache_slot, ce,
    /// offset)` and the `prop_info` beside it (`zend_get_property_offset`,
    /// `Z/Zend/zend_object_handlers.c:386`, `:456`): php writes the class
    /// into the slot the opline names, the offset into the next and the
    /// property's info, there only where it is typed, into the third, and
    /// its handler reads the place where the object's class is the one
    /// written, a declared slot in place and a dynamic property by one
    /// lookup of the name's known hash. Written only for a class served
    /// by the standard handlers, a declared property with no hooks and a
    /// scope that reaches it, and a name no class of the chain declares
    /// ([`crate::api::request::Request::cacheable_offset`]).
    ///
    /// `function` and `scope` are the frame's that placed it: php's
    /// cache is one per scope, and this engine's one per op array, so the
    /// slot is read back only from a frame of the same function, which
    /// fixes the scope, and for a frame of no function, an included or
    /// evaluated file's, only where its scope is `scope` too.
    Property {
        class: ClassId,
        function: Option<Function>,
        scope: Option<ClassId>,
        offset: PropertyOffset,
    },
    /// `ZEND_JMP_FL_MISS`: the table has the name in the namespace, so
    /// the jump is not taken and the call by name after it runs.
    Miss,
    /// `ZEND_JMP_FL_HIT`: the table has not the name in the namespace,
    /// so the jump lands on the frameless call over the global one.
    Hit,
}

/// What [`Cached::Property`] holds beside the class, php's `prop_offset`
/// in `CACHE_ADDR(cache_slot + 1)`: a declared slot, or
/// `ZEND_DYNAMIC_PROPERTY_OFFSET`, a name the object's dynamic table holds
/// if anything does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropertyOffset {
    /// `OBJ_PROP(zobj, prop_offset)`: the declared slot, and whether its
    /// property is typed, php's `prop_info` in the third slot.
    Declared { slot: u32, typed: bool },
    /// `IS_DYNAMIC_PROPERTY_OFFSET`: `zend_hash_find_known_hash` on
    /// `zobj->properties`.
    Dynamic,
}

/// `sizeof(void *)`: what `zend_alloc_cache_slot` grows `cache_size` by
/// for one slot, so a slot's number is its byte offset over this.
const SLOT_BYTES: u32 = 8;

/// The VM stack and its frames, and the calls being built.
#[derive(Default)]
pub(crate) struct Vm {
    /// The frames, each a [`Header`] and its slots on the VM stack.
    pub(crate) stack: Stack,
    /// php's `execute_fake` placeholders that outlive the generator frame
    /// they stood under, while what the frame held is released.
    placeholders: Vec<Placeholder>,
    /// The sites standing, `zend_get_attribute_object`'s dummy frames.
    sites: Vec<Site>,
    /// The switches standing, `EG(current_execute_data)` moved below the
    /// innermost frame.
    switches: Vec<Switch>,
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
pub(crate) fn find_function(req: &mut Request, name: Bare<'_>) -> Option<Function> {
    #[cfg(test)]
    {
        req.vm_mut().lookups = req.vm_mut().lookups.saturating_add(1);
    }
    req.function_named(name)
}

/// `zend_lookup_class_ex(name, NULL, ZEND_FETCH_CLASS_NO_AUTOLOAD)`
/// from a kernel whose opline owns a cache slot: the linked classes'
/// half of [`find_function`], made here and nowhere else so the tests
/// count what a primed slot spares. A caller hands over the lowered
/// half of the constant pair, which the table finds as it finds any
/// case.
pub(crate) fn find_class(req: &mut Request, name: Bare<'_>) -> Option<ClassId> {
    #[cfg(test)]
    {
        req.vm_mut().lookups = req.vm_mut().lookups.saturating_add(1);
    }
    req.class_named(name)
}

/// php's `execute_fake` placeholder standing where a generator's frame
/// stood, the frame gone from the stack while `zend_generator_close`
/// releases what it held, since `EG(current_execute_data)` is moved back
/// to the placeholder and not past it: a backtrace a destructor takes
/// still reads the generators the resumption came through
/// ([`crate::generator::placeholder_frames`]). `frame` is where on the
/// stack the generator's frame stood.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Placeholder {
    pub(crate) frame: usize,
    pub(crate) generator: PhpObject,
}

/// `zend_get_attribute_object`'s dummy call frame
/// (`Z/Zend/zend_attributes.c:340-370`): user code at a file and a line
/// of its own, the attribute's, which serves nothing and shows no trace
/// row (`ZEND_ACC_CALL_VIA_TRAMPOLINE`), so what is called from it is
/// called from there. It is the executing site while nothing stands
/// above it, the site the row above it names, and the caller whose
/// `strict_types` an argument above it is checked under. `frame` is
/// where it stands on the stack, as a [`Placeholder`]'s is.
#[derive(Debug, Clone)]
pub(crate) struct Site {
    pub(crate) frame: usize,
    pub(crate) file: Box<[u8]>,
    pub(crate) line: u32,
    /// `ZEND_ACC_STRICT_TYPES` on the dummy function, the attribute's
    /// `ZEND_ATTRIBUTE_STRICT_TYPES`.
    pub(crate) strict: bool,
}

/// `EG(current_execute_data)` moved to `to`, a frame below the
/// innermost, as a debugger moves it to the frame it evaluates code in
/// (xdebug's `xdebug_do_eval` around `zend_eval_stringl`, phpdbg's
/// `frame` before `ev`): from where the switch stands, `frame`, php's
/// `prev_execute_data` chain runs on at `to`, and the frames between
/// `to` and the switch are on no chain while it stands. What is pushed above it, the evaluated code's frame
/// first, links to `to` through it, as that frame's
/// `EX(prev_execute_data)` names `to`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Switch {
    pub(crate) frame: usize,
    pub(crate) to: FrameRef,
}

impl Vm {
    /// A fiber's: no frame, on a stack of the fiber's own pages.
    pub(crate) fn fiber() -> Vm {
        Vm {
            stack: Stack::fiber(),
            ..Vm::default()
        }
    }

    /// The switches standing, the innermost last.
    pub(crate) fn switches(&self) -> &[Switch] {
        &self.switches
    }

    /// A switch to `to` above everything standing, until
    /// [`Vm::pop_switch`].
    pub(crate) fn push_switch(&mut self, to: FrameRef) {
        self.switches.push(Switch {
            frame: self.stack.len(),
            to,
        });
    }

    pub(crate) fn pop_switch(&mut self) {
        self.switches.pop();
    }

    /// The place of the user frame `prev_execute_data` names below the
    /// place `above`, [`Vm::depth`] asking for the innermost: the frame
    /// under it on the stack, or the one a switch standing there moved
    /// `EG(current_execute_data)` to, which stands below it, past every
    /// call between, built or a native's served. None below the first.
    /// A frame being popped is passed over ([`CallInfo::leaving`]):
    /// `zend_leave_helper` has made its caller current before it frees
    /// the CVs, so what a release there raises, or calls, stands on the
    /// caller.
    pub(crate) fn below(&self, above: usize) -> Option<usize> {
        self.below_where(above, |info| info.user() && !info.leaving())
    }

    /// [`Vm::below`] over every record that is on php's chain, a
    /// native's call served among the user frames: what
    /// `EG(current_execute_data)` names from `above` down.
    pub(crate) fn below_record(&self, above: usize) -> Option<usize> {
        self.below_where(above, |info| !info.building() && !info.leaving())
    }

    /// The first record under `above`, through the switches, whose
    /// [`CallInfo`] `on` takes.
    fn below_where(&self, above: usize, on: impl Fn(CallInfo) -> bool) -> Option<usize> {
        let mut at = self.under(above)?;
        while !self.info_at(at).is_some_and(&on) {
            at = self.under(at)?;
        }
        Some(at)
    }

    /// The flags of the record at `at`.
    fn info_at(&self, at: usize) -> Option<CallInfo> {
        let at = u32::try_from(at).ok()?;
        Some(self.stack.header(FrameRef(at))?.info)
    }

    /// [`Vm::below`] with no frame passed over.
    fn under(&self, above: usize) -> Option<usize> {
        self.switches
            .iter()
            .rev()
            .find(|switch| switch.frame == above)
            .map(|switch| switch.to.0 as usize)
            .filter(|&to| to < above)
            .or_else(|| above.checked_sub(1))
    }

    /// The sites standing, the innermost last.
    pub(crate) fn sites(&self) -> &[Site] {
        &self.sites
    }

    /// The innermost site where no frame stands above it: what php's
    /// `EG(current_execute_data)` walk meets first as user code.
    pub(crate) fn innermost_site(&self) -> Option<&Site> {
        self.sites.last().filter(|site| {
            (site.frame..self.stack.len()).all(|at| self.info_at(at).is_some_and(CallInfo::call))
        })
    }

    /// A site at `file` and `line` above everything standing, until
    /// [`Vm::pop_site`].
    pub(crate) fn push_site(&mut self, file: &[u8], line: u32, strict: bool) {
        self.sites.push(Site {
            frame: self.stack.len(),
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
            frame: self.stack.len(),
            generator,
        });
    }

    pub(crate) fn pop_placeholder(&mut self) {
        self.placeholders.pop();
    }

    /// Push a frame of `slots` slots, with the room of `extra_args`
    /// extra arguments after them, its header laid down whole: its place
    /// is the `FrameRef` the VM stack gives it. The slots are every one
    /// `IS_UNDEF` until an opline or a placed argument writes it
    /// (`i_init_func_execute_data`), which is what a read of a CV warns
    /// of. A page the memory limit refuses is the fatal php raises there.
    #[inline]
    pub(crate) fn push(
        &mut self,
        heap: &mut php_heap::Heap,
        slots: usize,
        extra_args: usize,
        header: Header,
    ) -> Result<FrameRef, Fatal> {
        self.stack.push_frame(heap, slots, extra_args, header)
    }

    /// The frame of an unobserved frameless call, off the VM stack's
    /// pages ([`php_heap::VmStack::push_frameless`]): `slots` slots,
    /// every one `IS_UNDEF` until its operand is placed; more than three
    /// is the engine's error.
    pub(crate) fn push_frameless(
        &mut self,
        slots: usize,
        header: Header,
    ) -> Result<FrameRef, Fatal> {
        self.stack.push_frameless(slots, header)
    }

    /// A frame that was off the stack back on top of it, the half of a
    /// generator's resumption that is the engine's, and the calls its
    /// body was building when it yielded back on the stack above it,
    /// copied onto the pages as `zend_generator_restore_call_stack`
    /// copies them.
    pub(crate) fn resume(
        &mut self,
        heap: &mut php_heap::Heap,
        frame: HeapFrame,
        calls: Vec<HeapFrame>,
    ) -> Result<FrameRef, Fatal> {
        let at = self.stack.resume_frame(frame);
        for call in calls {
            self.stack.restore_frame(heap, call)?;
        }
        Ok(at)
    }

    /// The innermost frame off the stack, moved to an allocation of its
    /// own, with the calls it was building above it each moved to one,
    /// the outermost first: what `zend_generator_freeze_call_stack` takes
    /// with a generator that yields in the middle of a call's arguments.
    pub(crate) fn suspend(
        &mut self,
        heap: &mut php_heap::Heap,
    ) -> Result<Option<(HeapFrame, Vec<HeapFrame>)>, Fatal> {
        let mut calls = Vec::new();
        while self
            .stack
            .header(self.stack.current())
            .is_some_and(|header| header.info.building())
        {
            let Some(call) = self.stack.move_to_heap(heap)? else {
                break;
            };
            calls.push(call);
        }
        calls.reverse();
        let Some(frame) = self.stack.move_to_heap(heap)? else {
            return Ok(None);
        };
        Ok(Some((frame, calls)))
    }

    /// Every frame and call of this execution lent its receiver made its
    /// owner: the objects the caller is to take one owner of for each,
    /// and let one pin of each go.
    pub(crate) fn own_lent(&mut self) -> Vec<PhpObject> {
        let mut taken = Vec::new();
        for at in 0..self.stack.len() {
            let Some(header) = u32::try_from(at)
                .ok()
                .and_then(|at| self.stack.header_mut(FrameRef(at)))
            else {
                continue;
            };
            if let Some(Receiver::Lent(held)) = header.receiver() {
                header.set_receiver(Some(Receiver::Owned(held)));
                taken.push(held);
            }
        }
        taken
    }

    /// The place of the innermost user frame: the one the loop is
    /// executing, or the one a switch standing on top moved
    /// `EG(current_execute_data)` to ([`Switch`]), or the caller of a
    /// frame being popped; a native's call served above it passed over.
    pub(crate) fn innermost_at(&self) -> Option<usize> {
        self.below(self.stack.len())
    }

    /// `EG(current_execute_data)`: the innermost record on php's chain,
    /// a user frame or a native's call being served.
    pub(crate) fn current_at(&self) -> Option<FrameRef> {
        let at = self.below_record(self.stack.len())?;
        Some(FrameRef(u32::try_from(at).ok()?))
    }

    /// The record on top of the stack where it is the native's call
    /// [`Vm::serving_at`] answers and no switch stands, which leaves the
    /// walk nothing to pass over: the first record it reads is the top,
    /// and a native's call being served is neither being built nor
    /// being left. None otherwise, which the walk answers.
    #[inline]
    pub(crate) fn serving_top(&self) -> Option<&HeapFrame> {
        if !self.switches.is_empty() {
            return None;
        }
        let (_, frame) = self.stack.top()?;
        let info = frame.header().info;
        (info.serving_native() && !info.leaving()).then_some(frame)
    }

    /// The call being served where it is a native's, php's
    /// `execute_data` inside a native: [`Vm::current_at`] where that is
    /// one, and none while a user frame is current or a call is still
    /// being built.
    pub(crate) fn serving_at(&self) -> Option<FrameRef> {
        let at = self.current_at()?;
        self.at(at)
            .is_some_and(|header| header.info.serving_native())
            .then_some(at)
    }

    /// The call the `SEND_*` oplines are placing arguments into, the
    /// one on top of the stack while it is being built.
    pub(crate) fn building_at(&self) -> Option<FrameRef> {
        let (top, frame) = self.stack.top()?;
        frame.header().info.building().then_some(top)
    }

    /// The header of the call being built, mutably, what a
    /// `CHECK_FUNC_ARG` writes of `EX(call)`.
    #[inline]
    pub(crate) fn building_mut(&mut self) -> Option<&mut Header> {
        let (_, frame) = self.stack.top_mut()?;
        let header = frame.header_mut();
        header.info.building().then_some(header)
    }

    /// The header of the call being built ([`Vm::building_at`]), what a
    /// `SEND_*` reads of `EX(call)`.
    #[inline]
    pub(crate) fn building(&self) -> Option<&Header> {
        let (_, frame) = self.stack.top()?;
        let header = frame.header();
        header.info.building().then_some(header)
    }

    /// `EX(opline) = EG(exception_op)` on `frame`, or the frame back at
    /// an opline of its own ([`CallInfo::at_exception_op`]).
    pub(crate) fn stand_at_exception_op(&mut self, frame: FrameRef, at: bool) {
        if let Some(header) = self.stack.header_mut(frame) {
            header.info.set_at_exception_op(at);
        }
    }

    /// The header of the innermost frame ([`Vm::innermost_at`]).
    pub(crate) fn innermost(&self) -> Option<&Header> {
        let at = u32::try_from(self.innermost_at()?).ok()?;
        self.stack.header(FrameRef(at))
    }

    /// How many frames stand.
    pub(crate) fn depth(&self) -> usize {
        self.stack.len()
    }

    /// `Z_OBJ(EX(This))`: the receiver of `frame`, a view with no owner
    /// taken; none where the frame has none.
    pub(crate) fn this(&self, frame: FrameRef) -> Option<PhpObject> {
        self.at(frame).and_then(Header::this)
    }

    /// The header of `frame`, if it stands.
    pub(crate) fn at(&self, frame: FrameRef) -> Option<&Header> {
        self.stack.header(frame)
    }

    pub(crate) fn at_mut(&mut self, frame: FrameRef) -> Option<&mut Header> {
        self.stack.header_mut(frame)
    }

    /// Where `frame` is to write what it returns, the caller's slot, and
    /// nothing when the call that pushed it wants no value.
    pub(crate) fn returns_to(&self, frame: FrameRef) -> Option<(FrameRef, u32)> {
        let header = self.at(frame)?;
        Some((header.caller().frame()?, header.returns_to()?))
    }

    /// Where the loop stands in `frame`.
    pub(crate) fn opline(&self, frame: FrameRef) -> u32 {
        self.at(frame).map_or(0, |header| header.opline)
    }

    /// `CACHED_PTR(num)`: what the runtime cache of the op array `frame`
    /// serves holds at `slot`, read off the request's map table
    /// (`EX(run_time_cache)`, [`RuntimeCache::get`]).
    #[inline]
    pub(crate) fn cached(&self, map: &RuntimeCaches, frame: FrameRef, slot: CacheSlot) -> Cached {
        self.at(frame)
            .and_then(|header| map.get(header.runtime_cache()))
            .map_or(Cached::Unprimed, |cache| cache.get(slot))
    }

    /// `CACHE_PTR(num, ptr)`: `value` into the runtime cache of the op
    /// array `frame` serves at `slot`, in the request's map table
    /// ([`RuntimeCache::set`]).
    pub(crate) fn prime(
        &self,
        map: &mut RuntimeCaches,
        frame: FrameRef,
        slot: CacheSlot,
        value: Cached,
    ) {
        if let Some(header) = self.at(frame) {
            map.map(header.runtime_cache()).set(slot, value);
        }
    }

    /// `ZEND_OBSERVER_DATA(EX(func))`: the observer slot of the op array
    /// `frame` serves, in the request's map table; unresolved for a frame
    /// that stands nowhere.
    pub(crate) fn observer_slot(
        &self,
        map: &RuntimeCaches,
        frame: FrameRef,
    ) -> crate::observer::Slot {
        self.observer_slot_held(map, frame)
            .cloned()
            .unwrap_or_default()
    }

    /// [`Vm::observer_slot`] borrowed where it stands, for the walks that
    /// read one handler of it at a time.
    pub(crate) fn observer_slot_held<'map>(
        &self,
        map: &'map RuntimeCaches,
        frame: FrameRef,
    ) -> Option<&'map crate::observer::Slot> {
        self.at(frame)
            .and_then(|header| map.get(header.runtime_cache()))
            .map(|cache| &cache.observer)
    }

    /// The observer slot of the op array `frame` serves, resolved to
    /// `slot` (`zend_observer_fcall_install`).
    pub(crate) fn set_observer_slot(
        &self,
        map: &mut RuntimeCaches,
        frame: FrameRef,
        slot: crate::observer::Slot,
    ) {
        if let Some(header) = self.at(frame) {
            map.map(header.runtime_cache()).observer = slot;
        }
    }

    /// `EX(opline) = opline`, php's `SAVE_OPLINE()`: the frame's state
    /// records that it stands at `at`. The loop holds the position of
    /// the frame it executes in its own hand ([`crate::tier::Next`]) and
    /// writes it here where something outside the loop can read it; a
    /// frame the loop is not executing, a caller under a call or a
    /// generator's, is moved here directly.
    pub(crate) fn stand(&mut self, frame: FrameRef, at: u32) {
        if let Some(header) = self.at_mut(frame) {
            header.opline = at;
        }
    }

    /// `ZEND_VM_NEXT_OPCODE`: on to the opline after this one.
    pub(crate) fn advance(&mut self, frame: FrameRef) {
        if let Some(header) = self.at_mut(frame) {
            header.opline = header.opline.saturating_add(1);
        }
    }
}

/// What `frame` is serving, held by its own share of the unit so the
/// oplines can be read while the request is borrowed.
pub(crate) fn standing(req: &Request, frame: FrameRef) -> Result<Serving, Fatal> {
    req.serving(frame).ok_or_else(|| {
        Fatal::error(format!(
            "InternalError: frame {} is serving no op array",
            frame.0
        ))
    })
}

/// The op array `frame` is serving, out of its unit.
pub(crate) fn serving_code(serving: &Serving, frame: FrameRef) -> Result<&OpArray, Fatal> {
    serving.code().ok_or_else(|| {
        Fatal::error(format!(
            "InternalError: frame {} names a body its unit has not",
            frame.0
        ))
    })
}

/// The innermost `try` element `op_num` stands in, the one
/// `ZEND_HANDLE_EXCEPTION` and a generator's forced close unwind from.
pub(crate) fn innermost_try(code: &OpArray, op_num: u32) -> Option<u32> {
    let mut current = None;
    for (at, element) in code.try_catch.iter().enumerate() {
        if op_num < element.try_op.0 {
            break;
        }
        if op_num < element.catch_op.0 || op_num < element.finally_end.0 {
            current = u32::try_from(at).ok();
        }
    }
    current
}

/// The main op array of `unit`, what the frame `serve` pushes serves.
pub(crate) fn code_of(unit: &Unit) -> &OpArray {
    &unit.main
}

/// The record of `unit`'s main op array as another process reads
/// it: a file's, no name, php's `NULL` `function_name`.
fn code_record(unit: &Unit) -> crate::readable::Record {
    crate::readable::Record::user(&[], code_of(unit))
}

/// The op array `body` names in `unit`; none for an index the unit has
/// not.
pub(crate) fn body_of(unit: &Unit, body: Body) -> Option<&OpArray> {
    match body {
        Body::Main => Some(&unit.main),
        Body::Function(at) => unit.functions.get(at as usize).map(|(_, code)| code),
        Body::Dynamic(at) => unit.dynamic.get(at as usize).map(|(_, code)| code),
        Body::Method(at) => unit.methods.get(at as usize).map(|m| &m.body),
        Body::Hook(at) => unit.hooks.get(at as usize).map(|h| &h.body),
    }
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
                Err(php_heap::Fatal::error(format!(
                    "InternalError: {opcode} over $this on a frame with no receiver"
                )))
            },
            |this| Ok(this.as_value()),
        ),
        OperandKind::Const | OperandKind::Tmp => Err(php_heap::Fatal::error(format!(
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
            owned_deref(req, value)
        }
        OperandKind::Const | OperandKind::Cv | OperandKind::Unused => {
            let value = operand(req, frame, code, kind, field).deref();
            req.addref(&value);
            value
        }
    }
}

/// `ZVAL_DEREF` over a value one owner of which the caller holds: what
/// the reference held, with the owner moved onto it.
pub(crate) fn owned_deref(req: &mut Request, value: Value) -> Value {
    let Some(cell) = value.as_php_ref().copied() else {
        return value;
    };
    let held = cell.get();
    req.addref(&held);
    req.release(value);
    held
}

/// A hash key as a value of its own, what a fetch writes to its
/// `result` (`RETURN_VALUE_USED`) and a generator delegating to an
/// array takes as its key: an integer as it stands, and a string with
/// one more owner (`ZVAL_STR_COPY`).
pub(crate) fn key_value(req: &mut Request, key: php_heap::Key) -> Value {
    match key {
        php_heap::Key::Long(n) => Value::int(n),
        php_heap::Key::Str(s) => {
            let s = Value::string(PhpString::of(s));
            req.addref(&s);
            s
        }
    }
}

/// `GET_OP*_ZVAL_PTR_DEREF(BP_VAR_R)` as a frameless call reads its
/// operand into its record (`ZEND_FRAMELESS_ICALL_*`,
/// `Z/Zend/zend_vm_def.h:9786`): the zval as it stands, with an owner
/// taken only where the operand needs one to stand for the call. A
/// `TMP` or a `VAR` is moved, its slot left `UNDEF`, and the record's
/// owner is the one php's `FREE_OP*` gives up after the call; a `CV`'s
/// or a `CONST`'s value that counts owners takes one more, since the
/// native may serve code that writes over the variable while it reads
/// the value, and one that counts none takes nothing and is given up
/// as nothing. A `CV` nothing wrote, and a reference in any slot, are
/// read as [`take_operand`] reads them, the warning and the value the
/// reference holds.
///
/// The owner a `CV`'s string takes keeps it for the call when the
/// autoloader the native serves writes over the variable, and a
/// reference is read through:
///
/// ```php
/// spl_autoload_register(function ($c) { $GLOBALS['n'] = null; echo "load $c\n"; });
/// $n = str_repeat("Zz", 2);
/// var_dump(class_exists($n), $n);
/// $s = str_repeat("ab", 2); $r = &$s;
/// var_dump(str_contains($r, "ba"), strtr($r, "a", "x"), $s);
/// ```
/// ```expect
/// load ZzZz
/// bool(false)
/// NULL
/// bool(true)
/// string(4) "xbxb"
/// string(4) "abab"
/// ```
#[inline]
pub(crate) fn frameless_operand(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    kind: OperandKind,
    field: u32,
) -> Zval {
    let at = match kind {
        OperandKind::Const => {
            let z = code
                .literals
                .get(field as usize)
                .copied()
                .unwrap_or_else(Zval::null);
            z.addref();
            return z;
        }
        OperandKind::Cv => field as usize,
        OperandKind::Tmp | OperandKind::Var => temporary_slot(code, field) as usize,
        OperandKind::Unused => return take_operand(req, frame, code, kind, field).to_zval(),
    };
    let Some(slot) = req.slots_mut(frame).get_mut(at) else {
        return take_operand(req, frame, code, kind, field).to_zval();
    };
    let z = *slot;
    if z.is_undef() || z.kind() == php_heap::ZvalType::Reference {
        return take_operand(req, frame, code, kind, field).to_zval();
    }
    if kind == OperandKind::Cv {
        z.addref();
    } else {
        *slot = Zval::UNDEF;
    }
    z
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
/// the handler takes an owner of, lowers as `get_method` does with no
/// key ([`MethodName::lowered`]), and gives up with
/// [`MethodName::release`].
pub(crate) struct MethodName {
    pub(crate) written: Value,
    /// The second constant of a constant name's pair, the name lowered,
    /// `RT_CONSTANT(opline, opline->op2) + 1`, which the unit holds;
    /// none for a name the program computed.
    lowered_constant: Option<Value>,
    owned: bool,
}

/// A name lowered for a table keyed by the lowered name, as php has
/// one: the compiler's lowered constant, or a name with no upper-case
/// byte, borrowed (`zend_string_tolower` answers such a name itself);
/// otherwise lowered into a buffer on the stack where it fits, as
/// `zend_std_get_method` lowers a name it was given no key for
/// (`ZSTR_ALLOCA_ALLOC`, `zend_str_tolower_copy`), and allocated only
/// past [`INLINE_NAME`] bytes, where php's `do_alloca` goes to the
/// heap too. A name is found under any case whichever of the three
/// holds it, a name past the stack's bytes too:
///
/// ```php
/// class C {
///     function hello() { return "hello"; }
///     static function make() { return "make"; }
///     function aMethodWhoseNameIsLongerThanSixtyFourBytesSoItsLoweredFormIsAllocated() { return "long"; }
/// }
/// $o = new C; $n = 'HeLLo'; $s = 'MAKE';
/// $long = 'AMETHODWHOSENAMEISLONGERTHANSIXTYFOURBYTESSOITSLOWEREDFORMISALLOCATED';
/// echo $o->$n(), " ", [$o, 'HELLO'](), " ", C::$s(), " ", ['C', 'Make'](), " ", $o->$long(), "\n";
/// var_dump(method_exists($o, 'HELLO'), method_exists('C', $long), is_callable([$o, 'Nope']));
/// ```
/// ```expect
/// hello hello make make long
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
pub(crate) enum Lowered<'a> {
    Borrowed(&'a [u8]),
    Inline { bytes: [u8; INLINE_NAME], len: u8 },
    Allocated(Vec<u8>),
}

/// How many bytes a name [`Lowered`] lowers on the stack holds at most.
const INLINE_NAME: usize = 64;

impl<'a> Lowered<'a> {
    /// `name` lowered, borrowed where it has no upper-case byte.
    #[inline]
    pub(crate) fn of(name: &'a [u8]) -> Lowered<'a> {
        if !name.iter().any(u8::is_ascii_uppercase) {
            return Lowered::Borrowed(name);
        }
        match u8::try_from(name.len()) {
            Ok(len) if name.len() <= INLINE_NAME => {
                let mut bytes = [0; INLINE_NAME];
                for (into, byte) in bytes.iter_mut().zip(name) {
                    *into = byte.to_ascii_lowercase();
                }
                Lowered::Inline { bytes, len }
            }
            _ => Lowered::Allocated(name.to_ascii_lowercase()),
        }
    }
}

impl std::ops::Deref for Lowered<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Lowered::Borrowed(name) => name,
            Lowered::Inline { bytes, len } => bytes.get(..usize::from(*len)).unwrap_or_default(),
            Lowered::Allocated(name) => name,
        }
    }
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
            return Ok(MethodName {
                written: constant(code, ConstIdx(op.op2)),
                lowered_constant: Some(constant(code, ConstIdx(op.op2.saturating_add(1)))),
                owned: false,
            });
        }
        let value = operand_undef(req, frame, code, op.op2_type, op.op2);
        let held = value.deref();
        let name = if held.as_str_bytes().is_some() {
            req.addref(&held);
            Ok(MethodName {
                written: held,
                lowered_constant: None,
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

    /// The name lowered: the pair's lowered constant, borrowed, or the
    /// computed name lowered as [`Lowered::of`] lowers it.
    pub(crate) fn lowered(&self) -> Lowered<'_> {
        match &self.lowered_constant {
            Some(lowered) => Lowered::Borrowed(lowered.as_str_bytes().unwrap_or_default()),
            None => Lowered::of(self.written.as_str_bytes().unwrap_or_default()),
        }
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
        let set = req.declared_string(&var.name.0).and_then(|name| {
            let name = name.into_key();
            table.set(req, name, Owner::take(req, value))
        });
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
/// Every call a user function is reached by crosses it: the one an
/// opline pushed, which the loop serves as it takes the push
/// ([`crate::tier`]), and the one a native served
/// ([`Request::serve_user`]), here, so a `ReflectionMethod::invoke`
/// verifies what `DO_UCALL` verifies, and the coercion a scalar
/// parameter carries outside `strict_types` is served there too:
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
#[inline]
pub(crate) fn parameter_prologue(
    req: &mut Request,
    activated: Activated,
) -> Result<Option<FrameRef>, php_heap::Fatal> {
    // A frame whose push skipped every `RECV*` stands at its entry
    // already, which its activation told, and its op array is not held
    // for a run of none.
    if !activated.prologue {
        return Ok(None);
    }
    let serving = standing(req, activated.frame)?;
    let code = serving_code(&serving, activated.frame)?;
    let told = <crate::observer::Declared as crate::observer::Telling>::of(req);
    prologue(req, activated.frame, code, told)
}

/// The entry safepoint of `code`, `safepoints[0]`: the first opline past
/// the `RECV*` prologue (design §5.6), zero for an op array with none.
pub(crate) fn entry_of(code: &OpArray) -> u32 {
    code.safepoints.first().map_or(0, |at| at.0)
}

/// A call made a user frame ([`Request::activate_user`]): the frame, and
/// whether the activation left `RECV*` oplines of its prologue to serve
/// before the entry safepoint ([`entry_of`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Activated {
    pub(crate) frame: FrameRef,
    pub(crate) prologue: bool,
}

/// The `RECV` oplines from `start` in `code`, the op array `callee`
/// serves, that php's handler passes on its first test, read off the
/// frame once: each a parameter the call passed an argument for, not
/// by reference, of a type the mask the compiler put in `op2` holds
/// (`ZEND_RECV`'s `opline->op2.num & (1u << Z_TYPE_P(param))`,
/// `MAY_BE_ANY` for a parameter with no type). The answer is the first
/// opline that does not pass, where the handlers take over, at most the
/// entry safepoint; nothing observes these, so no observer is told.
#[inline]
pub(crate) fn recv_passed(req: &Request, callee: FrameRef, code: &OpArray, start: u32) -> u32 {
    let entry = entry_of(code);
    let Some(frame) = req.vm().stack.frame(callee) else {
        return start;
    };
    let passed = frame.header().num_args;
    let slots = frame.slots();
    let mut at = start;
    while at < entry {
        let Some(op) = code.oplines.get(at as usize) else {
            break;
        };
        if op.opcode != php_unit::Opcode::Recv || op.op1 > passed {
            break;
        }
        let Some(argument) = slots.get(op.out as usize) else {
            break;
        };
        let kind = argument.kind();
        if kind == php_heap::ZvalType::Reference || op.op2 & (1u32 << (kind as u32)) == 0 {
            break;
        }
        at += 1;
    }
    at
}

/// The `RECV*` oplines of [`parameter_prologue`] from where `callee`
/// stands up to the entry safepoint of `code`, the op array it serves,
/// the observers of a declared opcode told of each as `told` says: the
/// loop's own choice where the loop serves the push, so an engine where
/// no extension declared an opcode tests nothing here either (design
/// §5.8, #2287).
#[inline(never)]
pub(crate) fn prologue<D: crate::observer::Telling>(
    req: &mut Request,
    callee: FrameRef,
    code: &OpArray,
    told: D,
) -> Result<Option<FrameRef>, php_heap::Fatal> {
    let entry = entry_of(code);
    loop {
        let at = req.vm().opline(callee);
        if at >= entry {
            break;
        }
        let Some(prologue) = opline_at(code, at) else {
            break;
        };
        told.before(req, callee, at, prologue.opcode);
        let next = match prologue.opcode {
            php_unit::Opcode::Recv => {
                crate::handlers::recv::<crate::tier::Nop>(req, callee, code, prologue)?
            }
            php_unit::Opcode::RecvInit => {
                crate::handlers::recv_init::<crate::tier::Nop>(req, callee, code, prologue)?
            }
            php_unit::Opcode::RecvVariadic => {
                crate::handlers::recv_variadic::<crate::tier::Nop>(req, callee, code, prologue)?
            }
            other => {
                return Err(php_heap::Fatal::error(format!(
                    "InternalError: {} stands before the entry safepoint, and this engine serves no handler for it yet",
                    other.name()
                )));
            }
        };
        // The frame is not the loop's yet, so where a `RECV` leaves it is
        // written to its state, which the next one and the activation read.
        if let crate::tier::Stepped::Threw(at) = crate::tier::settled(req, callee, at, next) {
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
        // `ZEND_LIVE_ROPE`: the pieces converted so far, released.
        php_unit::LiveKind::Rope => {
            if let Some(rope) = garbage.as_int().and_then(RopeAt::of) {
                req.rope_close(rope);
            }
            return;
        }
        php_unit::LiveKind::TmpVar | php_unit::LiveKind::Loop => {}
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

/// `zend_unfinished_execution_gc_ex` (`Z/Zend/zend_execute.c:4968`):
/// what one frame that is not running holds, named for the collector.
/// The receiver and the closure its call released, the CVs where it has
/// no symbol table, the extra arguments and the extra named ones, the
/// calls it was building ([`unfinished_calls_gc`]) and the temporaries
/// and loop variables live where it stands; and its symbol table, which
/// php returns for its caller to hand the collector as a table, the CVs
/// reached through its `IS_INDIRECT` rows. Every frame the collector
/// reads is walked here, a suspended generator's and each frame of a
/// suspended fiber alike.
pub(crate) fn unfinished_execution_gc(
    state: FrameAt<'_>,
    calls: &[FrameAt<'_>],
    visit: &mut dyn FnMut(Value),
) -> Option<TableId> {
    if let Some(this) = state.receiver().and_then(Receiver::owned) {
        visit(this.as_value());
    }
    if let Some(closure) = state.closure() {
        visit(closure.as_value());
    }
    let code = state.code()?;
    let slot = |at: u32| Value::of_zval(state.frame.slot(php_heap::Slot(at)));
    let table = state.symbol_table();
    if table.is_none() {
        for cv in 0..code.num_cvs {
            visit(slot(cv));
        }
    }
    for extra in state.extra_args() {
        visit(extra);
    }
    for (_, value) in state.extra_named() {
        visit(*value);
    }
    unfinished_calls_gc(calls, visit);
    let opline = state.opline();
    if opline != 0 {
        for range in &code.live_ranges {
            if range.start.0 > opline {
                break;
            }
            if opline < range.end.0
                && matches!(
                    range.kind,
                    php_unit::LiveKind::TmpVar | php_unit::LiveKind::Loop
                )
            {
                visit(slot(temporary_slot(code, range.var)));
            }
        }
    }
    table
}

/// `zend_unfinished_calls_gc`: the calls a frame was building,
/// innermost first, each with the arguments it has placed, its
/// receiver, its extra named arguments and its closure.
pub(crate) fn unfinished_calls_gc(calls: &[FrameAt<'_>], visit: &mut dyn FnMut(Value)) {
    for call in calls.iter().rev() {
        for argument in call.arguments() {
            visit(argument);
        }
        if let Some(this) = call.receiver().and_then(Receiver::owned) {
            visit(this.as_value());
        }
        for (_, value) in call.extra_named() {
            visit(*value);
        }
        if let Some(closure) = call.closure() {
            visit(closure.as_value());
        }
    }
}

/// `cleanup_unfinished_calls` (`Z/Zend/zend_execute.c`): the calls the
/// frame was still building or serving when the throwable was raised,
/// given up innermost first: every record standing above the frame,
/// since the frame's own calls are the ones pushed above it.
pub(crate) fn cleanup_unfinished_calls(req: &mut Request, frame: FrameRef) {
    while req.vm().depth() > frame.0 as usize + 1 {
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
        .and_then(|lowered| find_class(req, Bare::resolved(lowered)));
    if let (Some(slot), Some(class)) = (slot, found) {
        req.prime(frame, slot, Cached::Class(class));
    }
    found
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use php_heap::FrameRef;
    use php_unit::{CacheSlot, MapPtrOffset, OpArray, PublishedUnit, Unit};

    use super::{Body, CacheAt, Cached, RuntimeCaches, code_of, init_runtime_cache};
    use crate::api::call::Function;
    use crate::api::request::Request;
    use crate::engine::{Engine, RequestDescription};

    /// A unit whose main op array has no oplines and `cache_size` bytes
    /// of runtime cache.
    fn with_cache(cache_size: u32) -> PublishedUnit {
        Unit {
            main: OpArray {
                cache_size,
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None)
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
        let frame = req.pushed(PublishedUnit::clone(&unit));
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
        let mut map = RuntimeCaches::default();
        let mut heap = php_heap::Heap::new();
        let code = code_of(&unit);
        for _ in 0..2 {
            assert!(init_runtime_cache(&mut map, &mut heap, code, Body::Function(0)).is_ok());
        }
        let ptr = CacheAt::of(code.runtime_cache);
        let laid = map.get(ptr).and_then(|cache| cache.slots.as_ref());
        assert_eq!(
            laid.map(|slots| slots.len()),
            Some(1),
            "one slot of eight bytes"
        );
        map.map(ptr).set(CacheSlot(0), Cached::Miss);
        assert!(init_runtime_cache(&mut map, &mut heap, code, Body::Function(0)).is_ok());
        assert_eq!(
            map.get(ptr).map(|cache| cache.get(CacheSlot(0))),
            Some(Cached::Miss),
            "the second activation finds the first's slot"
        );
        assert!(init_runtime_cache(&mut map, &mut heap, code, Body::Main).is_ok());
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

    /// A unit compiled for the request alone takes a number of the
    /// request's for each op array and none of the engine's offsets, and
    /// once nothing holds it, the next such unit takes its numbers again,
    /// over caches emptied as php frees a heap cache with its op array.
    #[test]
    fn a_request_only_unit_s_numbers_are_the_request_s_and_handed_out_again() {
        let one_function = || Unit {
            main: OpArray {
                cache_size: 8,
                ..OpArray::default()
            },
            functions: vec![(
                php_unit::decl::FunctionDecl {
                    name: php_unit::decl::Name::from("f"),
                    parameters: Vec::new(),
                    return_type: None,
                    returns_by_ref: false,
                    is_generator: false,
                    attributes: Vec::new(),
                    doc_comment: None,
                    file: php_unit::decl::Name::default(),
                    line: 0,
                    end_line: 0,
                    frameless: Vec::new(),
                },
                OpArray {
                    cache_size: 8,
                    ..OpArray::default()
                },
            )],
            ..Unit::default()
        };
        let mut caches = RuntimeCaches::default();
        let taken = |caches: &mut RuntimeCaches| {
            let mut unit = one_function();
            caches.number(&mut unit);
            let unit = unit.published(None);
            caches.hold(&unit);
            unit
        };
        let first = taken(&mut caches);
        let numbers = |unit: &Unit| {
            std::iter::once(&unit.main)
                .chain(unit.functions.iter().map(|(_, code)| code))
                .map(|code| code.runtime_cache)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            numbers(&first),
            [MapPtrOffset::Request(0), MapPtrOffset::Request(1)]
        );
        let mut heap = php_heap::Heap::new();
        assert!(
            init_runtime_cache(&mut caches, &mut heap, code_of(&first), Body::Function(0)).is_ok()
        );
        let at = CacheAt::of(first.main.runtime_cache);
        caches.map(at).set(CacheSlot(0), Cached::Hit);
        let second = taken(&mut caches);
        assert_eq!(
            numbers(&second),
            [MapPtrOffset::Request(2), MapPtrOffset::Request(3)],
            "a unit still held keeps its numbers"
        );
        drop(first);
        let third = taken(&mut caches);
        let mut taken = numbers(&third);
        taken.sort_by_key(|at| match at {
            MapPtrOffset::Engine(n) | MapPtrOffset::Request(n) => *n,
        });
        assert_eq!(
            taken,
            [MapPtrOffset::Request(0), MapPtrOffset::Request(1)],
            "the numbers of a unit nothing holds are handed out again"
        );
        assert!(
            caches.get(at).is_some_and(|cache| cache.slots.is_none()),
            "over a cache emptied"
        );
    }
}
