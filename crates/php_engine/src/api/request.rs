//! The request-environment group (design §4.4, §5.14): what a native
//! reaches through `&mut Request`, the one parameter that says a function
//! may re-enter PHP code (principle 9, ADR-0002). Ninety-eight methods,
//! `api.lock`'s largest group. The skeleton answers each from what the
//! request holds; the body bytes and the header block are kept on the
//! request and handed to the sink by `serve`, which owns the commit
//! moment; `serve` says what the request came to.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::any::{Any, TypeId};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use php_compile::StackLimit;
use php_heap::{
    Arr, Fatal, FrameRef, Heap, HeapHooks, Intern, MapTable, Obj, Ref, Res, ResourceType, Slot,
    VmStack, Zval, ZvalRef,
};
use php_unit::decl::{
    ClassConstDecl, ClassDecl, ClassKind, FunctionDecl, MethodDecl, Name, Visibility,
};
use php_unit::{FramelessEntry, Opcode, Unit};

use crate::api::abi::{
    Callable, Dependency, Extension, IniVerdict, NativeNew, ObjectHandlers, PropertyTest, Std,
};
use crate::api::call::{Function, Target};
use crate::api::raising::{Level, Thrown};
use crate::api::values::{
    ClassId, ClosureInfo, Key, PhpArray, PhpObject, PhpRef, PhpString, Resource, TypeTag, Value,
    ValueRef,
};
use crate::engine::{RequestDescription, Shared};
use crate::exec::{self, CallStage, CallState, Vm};
use crate::output::Output;

/// php's version, what `PHP_VERSION` and `phpversion()` say.
pub const PHP_VERSION: &str = "8.5.9";

/// A path on the host's filesystem, absolute, after the request's `cwd`
/// and `open_basedir` have been applied.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct HostPath(pub PathBuf);

/// A linked extension, by identity: its position in the bundle's startup
/// order, which is where its `zend_module_entry` stands in
/// `module_registry`. `Core`, which php registers first, is no
/// `Extension` value of the bundle and has no identity here yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExtensionId(pub(crate) u32);

/// What a native intends to do at a path, for the `open_basedir` guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathIntent {
    Read,
    Write,
    Create,
    Remove,
    Stat,
    Exec,
}

/// One `ob_*` operation on the request's buffer stack.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputOp {
    Start {
        callback: Option<Callable>,
        chunk_size: i64,
        flags: i64,
    },
    Flush,
    Clean,
    EndFlush,
    EndClean,
    GetFlush,
    GetClean,
    GetContents,
    GetLength,
    GetLevel,
    GetStatus {
        full: bool,
    },
    ListHandlers,
    ImplicitFlush(bool),
}

/// An open stream: what `fopen` and `opendir` hand back, reached by id
/// through the request's stream table (design §4.3 seam 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Stream {
    pub(crate) id: u32,
}

/// A stream wrapper's operations, Zend's `php_stream_wrapper_ops` as a
/// value: the scheme it serves and one function per operation, `None`
/// where the wrapper does not support it.
#[derive(Debug, Clone)]
#[allow(clippy::type_complexity)] // Zend's ops struct: a table of function pointers
pub struct StreamWrapper {
    /// The protocol, as `php_register_url_stream_wrapper` is given it.
    pub scheme: String,
    pub open: fn(&mut Request, &[u8], &[u8]) -> Result<Stream, Thrown>,
    pub open_dir: Option<fn(&mut Request, &[u8]) -> Result<Stream, Thrown>>,
    pub stat: Option<fn(&mut Request, &[u8]) -> Result<Option<PhpArray>, Thrown>>,
    pub unlink: Option<fn(&mut Request, &[u8]) -> Result<bool, Thrown>>,
    pub rename: Option<fn(&mut Request, &[u8], &[u8]) -> Result<bool, Thrown>>,
    pub mkdir: Option<fn(&mut Request, &[u8], i64, bool) -> Result<bool, Thrown>>,
    pub rmdir: Option<fn(&mut Request, &[u8]) -> Result<bool, Thrown>>,
}

/// A stream filter: its name and the function that filters one bucket,
/// with `closing` set on the last.
#[derive(Debug, Clone)]
#[allow(clippy::type_complexity)] // as StreamWrapper
pub struct StreamFilter {
    /// The filter's name, as `php_stream_filter_register_factory` is
    /// given it.
    pub name: String,
    pub filter: fn(&mut Request, &[u8], &mut Vec<u8>, bool) -> Result<(), Thrown>,
}

/// A resource's per-type destructor, php's `list_dtor_ex`: served once,
/// by [`Resource::close`] or by the release that reaches zero, with the
/// resource already closed and its payload still readable.
pub type ResourceDtor = fn(&mut Request, Resource);

/// The resource types this request knows, php's `list_destructors`
/// (`zend_register_list_destructors_ex`): a name and a destructor per
/// type, numbered in registration order. The list itself,
/// `EG(regular_list)`, is the heap's, reached through
/// [`Request::resource`].
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

    /// The type's name, `zend_rsrc_list_get_rsrc_type` over the table.
    fn type_name(&self, kind: ResourceType) -> Option<&[u8]> {
        self.types
            .get(kind.0 as usize)
            .map(|(name, _)| name.as_slice())
    }

    /// The type's destructor, if it registered one.
    fn dtor(&self, kind: ResourceType) -> Option<ResourceDtor> {
        self.types.get(kind.0 as usize).and_then(|(_, dtor)| *dtor)
    }
}

/// One request's interrupt (design §5.8; `CONTEXT.md`, Interrupt), as
/// the thread that raises it sees it: `Send` and `Sync`, and cloned so
/// that the engine's execution timer and the thread serving the request
/// share one. Raising is all a setter does; acting on it is the loop's,
/// at a back-edge, at a frame's activation and after every
/// `Flow::Resume`, and nothing else touches it.
#[derive(Debug, Clone, Default)]
pub struct Interrupt(Arc<State>);

impl Interrupt {
    /// What signal delivery raises it with: the loop dispatches what was
    /// delivered to its PHP handlers at its next check, which is php's
    /// `zend_interrupt_function`.
    pub fn raise(&self) {
        self.0.raised.store(true, Ordering::Release);
    }

    /// Whether it is raised: what the loop tests at its three safepoints
    /// and what compiled code tests once per loop iteration through the
    /// tier's `ExitCtx` (`jit-design.md` §4.3).
    #[must_use]
    pub fn raised(&self) -> &AtomicBool {
        &self.0.raised
    }

    /// What the engine's execution timer raises it with,
    /// `max_execution_time` having elapsed: the seconds it was armed
    /// with, then the two words `zend_timeout_handler` stores
    /// (`Z/Zend/zend_execute_API.c:1516`). The seconds go down first and
    /// the store that raises it releases them, because php's handler
    /// runs on the interrupted thread and this timer does not.
    pub(crate) fn elapsed(&self, seconds: i64) {
        self.0.seconds.store(seconds, Ordering::Relaxed);
        self.0.timed_out.store(true, Ordering::Relaxed);
        self.0.raised.store(true, Ordering::Release);
    }

    /// What unsetting the execution timer does beside stopping it
    /// (`zend_unset_timeout`, `Z/Zend/zend_execute_API.c:1657-1685`): the
    /// timer's word is cleared, so a deadline the old timer reached as
    /// it was being unset is not the fatal the new one raises.
    pub(crate) fn timer_unset(&self) {
        self.0.timed_out.store(false, Ordering::Relaxed);
    }

    /// What a check that found it raised reads: it is cleared, and the
    /// seconds come back when the execution timer was what raised it,
    /// both as php clears them (`Z/Zend/zend_vm_def.h:10526`, `:10537`).
    /// Clearing it acquires what the setter released, so the seconds the
    /// text names are the ones the timer was armed with; the loop's own
    /// test stays a relaxed load, because this is where the two threads
    /// meet.
    pub(crate) fn taken(&self) -> Option<i64> {
        self.0.raised.swap(false, Ordering::Acquire);
        self.0
            .timed_out
            .swap(false, Ordering::Relaxed)
            .then(|| self.0.seconds.load(Ordering::Relaxed))
    }
}

/// What an [`Interrupt`] holds, one per request: the atomic the loop
/// tests, which of the two setters raised it, and the seconds php's
/// timeout text names.
#[derive(Debug, Default)]
struct State {
    raised: AtomicBool,
    /// php's `EG(timed_out)`.
    timed_out: AtomicBool,
    /// php's `EG(timeout_seconds)`.
    seconds: AtomicI64,
}

/// How the request is ending, when it is: a fatal, a source that did
/// not compile, or an `exit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ending {
    /// `E_ERROR`: the fatal as `php_error_cb` gave it to `php_log_err`,
    /// one entry, which is the text `serve` reports and not a second
    /// rendering; the streams already have theirs.
    Fatal(String),
    /// `E_PARSE`: the text is already on the stream `php_error_cb` put it
    /// on, and what is left is php's status, 255.
    Parse,
    Exit(i32),
}

/// What a request wrote beside the body, one call the sink will see:
/// an entry for the log or bytes for the error stream. Kept in the
/// order they were written, since under php's CLI the two land on one
/// fd, and there the log form of a diagnostic stands before its
/// `display_errors=stderr` form (#216).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Aside {
    /// `OutputSink::log`: one entry, whole, ended by the host.
    Log(Vec<u8>),
    /// `OutputSink::write_stderr`: bytes as given.
    Stderr(Vec<u8>),
}

/// One request (design §5.14): `Send`, single-threaded, dropped at the
/// end of `serve`. Everything a native may touch is a method here or on
/// a value it hands out.
pub struct Request {
    shared: Arc<Shared>,
    description: RequestDescription,
    heap: Heap,
    stack: VmStack,
    vm: Vm,
    /// The map table of design §5.14, laid down with the request and
    /// read once a published unit has map-pointer offsets to give it.
    #[allow(dead_code)]
    map: MapTable,
    states: Vec<(TypeId, Box<dyn Any + Send>)>,
    ini: Vec<(Vec<u8>, Vec<u8>)>,
    /// What passed the `php_output` layer, php's `sapi_module.ub_write`: the
    /// bytes the host will be handed, kept until `serve` hands them.
    body: Vec<u8>,
    /// The `php_output` layer, `main/output.c`'s `OG` and the header state
    /// `sapi_send_headers` keeps beside it (`crate::output`).
    output: Output,
    header_callback: Option<Callable>,
    /// The host's log as `php_log_err` was given it, one entry per call,
    /// a diagnostic whole however many lines its backtrace adds, and no
    /// terminator, since that is the host's (`sapi_cli_log_message`
    /// writes `"%s\n"`, a file log stamps `[date] ` and `PHP_EOL`); and
    /// the error stream's bytes as `fputs(stderr)` was given them; the
    /// two in the order they were written.
    aside: Vec<Aside>,
    last_error: Option<(Level, String, Vec<u8>, u32)>,
    /// php's `EG(last_fatal_error_backtrace)`, rendered by
    /// `zend_trace_to_string`: the trace `zend_error_impl` fetched for the
    /// fatal being raised under `fatal_error_backtraces`, cleared for
    /// every diagnostic and by `zend_exception_error`, read by
    /// `php_error_cb` for the display and the log forms.
    last_fatal_error_backtrace: Option<String>,
    ending: Option<Ending>,
    resources: Resources,
    superglobals: Vec<(Vec<u8>, PhpArray)>,
    /// php's `EG(symbol_table)`, the main op array's variables (design
    /// §5.6, §5.10): its CVs, which `zend_detach_symbol_table` moves
    /// here in the order the compiler numbered them, each name once and
    /// this table their one owner. It outlives the frame, so a variable
    /// the program wrote at the top level is still an owner while the
    /// shutdown functions are served; `shutdown_destructors` walks the
    /// table in reverse before the store's walk, and
    /// `zend_shutdown_executor_values` destroys it after the
    /// extensions' `rshutdown`. `$GLOBALS`, `compact`, `extract` and
    /// `get_defined_vars` read this table (design §5.6).
    symbols: Vec<(Vec<u8>, Value)>,
    constants: Vec<(Vec<u8>, Value)>,
    /// `zend_enum_get_case`'s slot (`Z/Zend/zend_enum.c:410`): one
    /// instance per case of each enum, made on the first read of the
    /// case and handed back by identity from then on, so that
    /// `E::A === E::A`. php keeps it in the class's constant table,
    /// which outlives the request; the linked tables here are shared
    /// and immutable and an object is neither, so the slot is the
    /// request's and the cases go with it.
    enum_cases: Vec<((ClassId, Vec<u8>), PhpObject)>,
    wrappers: Vec<StreamWrapper>,
    filters: Vec<StreamFilter>,
    error_handlers: Vec<Option<Callable>>,
    exception_handlers: Vec<Option<Callable>>,
    shutdown_functions: Vec<(Callable, Vec<Value>)>,
    tick_functions: Vec<(Callable, Vec<Value>)>,
    autoloaders: Vec<Callable>,
    assert_callback: Option<Value>,
    umask: u32,
    cwd: PathBuf,
    gc_enabled: bool,
    streams: u32,
    /// The caller file's `declare(strict_types=1)`.
    strict_types: bool,
    /// php's `EG(exception)`: the throwable a handler left pending, which
    /// the loop reads at the `Threw` edge.
    pending: Option<PhpObject>,
    /// `zend_objects_store_mark_destructed`: every object behind the
    /// request, set where `shutdown_destructors` catches the
    /// `zend_bailout` a destructor's raise ends the walk with
    /// (`Z/Zend/zend_execute_API.c:258-266`). No destructor is served
    /// once it stands.
    marked_destructed: bool,
    /// What the entry frame of the innermost serving returned.
    returned: Value,
    /// The interrupt, shared with whatever thread sets it (design §5.8).
    interrupt: Interrupt,
    /// The execution timer, armed while `max_execution_time` is not
    /// zero; dropping the request unsets it.
    timer: Option<crate::interrupt::Timer>,
    /// The thread's stack limit, php's `EG(stack_limit)`, worked out
    /// once at request start on the thread the request runs on
    /// (`zend_call_stack_init`) from the two `zend.*_stack_size`
    /// entries; what the compiler is held to (design §5.6).
    stack_limit: StackLimit,
    /// The classes the units this request served declared, each the
    /// unit that holds it and the index of its declaration, in the
    /// order declared: php's `EG(class_table)` past what the engine
    /// registered at startup. A `ClassId` past the engine's classes
    /// counts into this list. What design §6.2 binds early into an
    /// immutable linked class shared across requests is not built yet,
    /// so every class a unit declares is this request's, the
    /// `DECLARE_CLASS_DELAYED` path, and dropped with it.
    declared: Vec<(Arc<Unit>, u32)>,
    /// The functions and methods the units this request served declared,
    /// each the unit that holds it and which of its op arrays is the
    /// body: php's `EG(function_table)` past the natives and each user
    /// class's `function_table`. A `Function` past the natives and the
    /// native methods counts into this list (design §5.6).
    user_functions: Vec<UserFunction>,
}

/// One row of a backtrace before it is an array: the callee, its
/// receiver, the scope it was called in where the callee is a closure,
/// its arguments, and the file and line of its call site, which a call
/// a native made has none of. php reads the scope off
/// `func->common.scope` of the closure's own copy of the function; the
/// copy here is the `Closure` object the call stands on, so the scope
/// travels beside the callee.
type TraceRow = (
    Function,
    Option<PhpObject>,
    Option<ClassId>,
    Vec<Value>,
    Option<(Vec<u8>, i64)>,
);

/// What `zend_get_property_offset` places a declared instance property
/// by: the slot it sits in, the visibility a read is held to, the set
/// visibility a write is held to where it has one of its own, and
/// whether the property carries a type.
#[derive(Debug, Clone, Copy)]
struct DeclaredSlot {
    slot: u32,
    visibility: Visibility,
    set_visibility: Option<Visibility>,
    typed: bool,
}

/// One user function or method a request declared: the unit and the
/// body, from which the declaration and the op array are both read.
pub(crate) struct UserFunction {
    pub(crate) unit: Arc<Unit>,
    pub(crate) body: exec::Body,
}

/// The declaration of a user function the request's table holds, from
/// whichever table of its unit the body came out of: one the file
/// declared at its top level, or one a `DECLARE_FUNCTION` bound where
/// it stood. None for a method, whose declaration is its class's.
fn user_function_decl(user: &UserFunction) -> Option<&FunctionDecl> {
    match user.body {
        exec::Body::Function(at) => user
            .unit
            .functions
            .get(usize::try_from(at).ok()?)
            .map(|(decl, _)| decl),
        exec::Body::Dynamic(at) => user
            .unit
            .dynamic
            .get(usize::try_from(at).ok()?)
            .map(|(decl, _)| decl),
        exec::Body::Main | exec::Body::Method { .. } => None,
    }
}

impl Request {
    pub(crate) fn new(shared: Arc<Shared>, description: RequestDescription) -> Request {
        let cwd = description.cwd.clone();
        let web = description.is_web;
        let mut req = Request {
            shared,
            description,
            heap: Heap::new(),
            stack: VmStack::new(),
            vm: Vm::default(),
            map: MapTable::new(),
            states: Vec::new(),
            ini: Vec::new(),
            body: Vec::new(),
            output: Output::activate(web),
            header_callback: None,
            aside: Vec::new(),
            last_error: None,
            last_fatal_error_backtrace: None,
            ending: None,
            resources: Resources::default(),
            superglobals: Vec::new(),
            symbols: Vec::new(),
            constants: Vec::new(),
            enum_cases: Vec::new(),
            wrappers: Vec::new(),
            filters: Vec::new(),
            error_handlers: Vec::new(),
            exception_handlers: Vec::new(),
            shutdown_functions: Vec::new(),
            tick_functions: Vec::new(),
            autoloaders: Vec::new(),
            assert_callback: None,
            umask: 0o022,
            cwd,
            gc_enabled: true,
            streams: 0,
            strict_types: false,
            pending: None,
            marked_destructed: false,
            returned: Value::null(),
            interrupt: Interrupt::default(),
            timer: None,
            stack_limit: StackLimit::UNCHECKED,
            declared: Vec::new(),
            user_functions: Vec::new(),
        };
        // Request state in bundle order (ADR-0005).
        for extension in &req.shared.extensions {
            if let Some((type_id, build)) = &extension.request_state {
                req.states.push((*type_id, build()));
            }
        }
        // The execution timer, as `zend_set_timeout` arms one at the
        // start of a request (design §5.8), and the memory limit, as
        // `php_request_startup` sets one from the ini.
        req.arm_timer();
        req.set_memory_limit();
        req.stack_limit = req.shared.stack_limit();
        req
    }

    /// The thread's stack limit as request start read it, for the
    /// compiler of whatever this request compiles.
    pub(crate) fn stack_limit(&self) -> StackLimit {
        self.stack_limit
    }

    /// `zend_unset_timeout` then `zend_set_timeout` over
    /// `max_execution_time` as the ini table has it now: at request
    /// start, and again whenever the entry moves, which is php's
    /// `OnUpdateTimeout` at runtime. Whatever was armed is unset first,
    /// so the seconds count from here.
    fn arm_timer(&mut self) {
        self.timer = None;
        self.timer = crate::interrupt::Timer::arm(&self.interrupt, self.max_execution_time());
    }

    // -------------------------------------------------- crate-internal

    pub(crate) fn stack_mut(&mut self) -> &mut VmStack {
        &mut self.stack
    }

    /// The engine's half of the frames: what the loop and the handlers
    /// read (design §5.6).
    pub(crate) fn vm(&self) -> &Vm {
        &self.vm
    }

    pub(crate) fn vm_mut(&mut self) -> &mut Vm {
        &mut self.vm
    }

    /// Push a frame for `unit`'s op array: its shape on the VM stack and
    /// its engine state, which are pushed and popped together and share a
    /// `FrameRef`.
    pub(crate) fn push_frame(&mut self, unit: Arc<Unit>) -> FrameRef {
        let code = exec::code_of(&unit);
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        let frame = self.stack.push_frame(slots);
        self.vm.push(unit);
        frame
    }

    /// Push a frame for the body of the user function `function`, the
    /// call that reached it served (`zend_vm_stack_push_call_frame` and
    /// `i_init_func_execute_data`, design §5.6): the CVs and the
    /// temporaries as slots, the arguments the call placed written into
    /// the first CVs, which are the parameters, the ones past the
    /// declared parameters kept beside the frame, and the receiver and
    /// the called scope carried over; `returns_to` is the caller's slot
    /// for what the frame returns. None for a function that is no user
    /// function, which is the caller's internal error to name.
    pub(crate) fn push_user_frame(
        &mut self,
        function: Function,
        call: exec::CallState,
        returns_to: Option<(FrameRef, u32)>,
    ) -> Option<FrameRef> {
        let (unit, body) = {
            let user = self.user_function(function)?;
            (Arc::clone(&user.unit), user.body)
        };
        let code = exec::body_of(&unit, body)?;
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        // The declared parameters, which is php's `common.num_args` and
        // excludes a variadic: an argument from the variadic's position
        // on is an extra one, where php leaves it on the stack for
        // `RECV_VARIADIC` to collect.
        let parameters = self.function_decl(function).map_or(0, |decl| {
            decl.parameters.iter().filter(|p| !p.variadic).count()
        });
        let num_args = u32::try_from(call.arguments.len()).unwrap_or(u32::MAX);
        let closure = call.closure;
        let frame = self.stack.push_frame(slots);
        self.vm.push_body(
            unit,
            body,
            Some(function),
            call.this,
            call.called_scope,
            num_args,
            returns_to,
        );
        let mut extra = Vec::new();
        for (at, argument) in call.arguments.into_iter().enumerate() {
            if at < parameters {
                let previous =
                    self.set_slot(frame, u32::try_from(at).unwrap_or(u32::MAX), argument);
                self.release(previous);
            } else {
                extra.push(argument);
            }
        }
        if let Some(state) = self.vm.at_mut(frame) {
            state.extra_args = extra;
            state.closure = closure;
        }
        if let Some(name) = call.name {
            self.release(Value::string(name));
        }
        for coerced in call.coerced {
            self.release(coerced);
        }
        Some(frame)
    }

    /// The frame off both stacks, every owner its slots held given up
    /// while the frame still stands, `i_free_compiled_variables` over
    /// the CVs and the temporaries with them, since a temporary the
    /// compiler numbered for an opline's value and nothing read is
    /// still an owner here; the receiver (`ZEND_CALL_RELEASE_THIS`) and
    /// the extra arguments (`zend_vm_stack_free_extra_args`) with them;
    /// then `zend_vm_stack_free_call_frame`.
    pub(crate) fn pop_frame(&mut self) {
        let frame = self.stack.current();
        let slots = self.stack.frame(frame).map_or(0, php_heap::Frame::len);
        for at in 0..slots {
            let value = self.take_slot(frame, u32::try_from(at).unwrap_or(u32::MAX));
            self.release(value);
        }
        if let Some(state) = self.vm.pop() {
            if let Some(this) = state.this {
                self.release(this.as_value());
            }
            if let Some(closure) = state.closure {
                self.release(closure.as_value());
            }
            for extra in state.extra_args {
                self.release(extra);
            }
        }
        self.stack.pop_frame();
    }

    /// The value in a slot of `frame`, a view of it: `ZVAL_COPY_VALUE`,
    /// no owner taken, off the heap's frame (design §5.6). A slot past
    /// the end reads as `UNDEF`, as an uninitialised zval reads.
    pub(crate) fn slot(&self, frame: FrameRef, at: u32) -> Value {
        self.stack
            .frame(frame)
            .map_or(Value::undef(), |f| Value::of_zval(f.slot(Slot(at))))
    }

    /// The value out of a slot, the slot left `UNDEF`: what the slot
    /// owned is the caller's now, which is how a temporary is consumed.
    pub(crate) fn take_slot(&mut self, frame: FrameRef, at: u32) -> Value {
        let Some(f) = self.stack.frame_mut(frame) else {
            return Value::undef();
        };
        let z = f.slot(Slot(at));
        f.set(Slot(at), Zval::UNDEF);
        Value::of_zval(z)
    }

    /// `value` into a slot, the slot owning it from here, and what the
    /// slot held handed back for the caller to release; a slot past the
    /// end takes nothing and hands `value` back.
    pub(crate) fn set_slot(&mut self, frame: FrameRef, at: u32, value: Value) -> Value {
        let Some(f) = self
            .stack
            .frame_mut(frame)
            .filter(|f| (at as usize) < f.len())
        else {
            return value;
        };
        let old = f.slot(Slot(at));
        f.set(Slot(at), value.to_zval());
        Value::of_zval(old)
    }

    /// The zval in a slot of `frame` in place, php's `EX_VAR(var)`
    /// (design §5.6): what a `FETCH_*_W` points its `VAR` at, and what
    /// the `ASSIGN*` over that `VAR` reads through when an `IS_INDIRECT`
    /// stands there (`_get_zval_ptr_ptr_var`). None for a slot the
    /// frame does not have.
    pub(crate) fn slot_ptr(&mut self, frame: FrameRef, at: u32) -> Option<ZvalRef> {
        self.stack.frame_mut(frame)?.slot_ptr(Slot(at))
    }

    /// The served call off the stack, `zend_vm_stack_free_args` over what
    /// the `SEND_*` oplines placed and `zend_vm_stack_free_call_frame`
    /// after it.
    pub(crate) fn pop_call(&mut self) {
        if let Some(call) = self.vm.pop_call() {
            for value in call.coerced {
                self.release(value);
            }
            for value in call.arguments {
                self.release(value);
            }
            if let Some(name) = call.name {
                self.release(Value::string(name));
            }
            if let Some(this) = call.this {
                self.release(this.as_value());
            }
            if let Some(closure) = call.closure {
                self.release(closure.as_value());
            }
        }
    }

    /// `zend_call_function` over a native method: the call frame pushed
    /// as `INIT_METHOD_CALL` and the `SEND_*` oplines would push it, the
    /// receiver and each argument one more owner the frame holds, the
    /// entry served, and the frame popped whether it returned or threw.
    pub(crate) fn serve_method(
        &mut self,
        function: Function,
        this: Option<PhpObject>,
        called_scope: ClassId,
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        self.serve_call(function, this, Some(called_scope), None, arguments)
    }

    /// `zend_call_known_fcc` over a `Closure`, which is what
    /// `Closure::__invoke` serves: the payload's function on the
    /// payload's receiver and scope, with the closure itself on the
    /// call so the body's `BIND_STATIC` prologue finds the variables it
    /// bound (`ZEND_CALL_CLOSURE`). The closure is one more owner for
    /// the span of the call, as php's `zend_init_func_execute_data`
    /// takes one.
    pub(crate) fn serve_closure(
        &mut self,
        object: PhpObject,
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        let Some(closure) = object.closure(self).cloned() else {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                "Closure object cannot have properties",
            ));
        };
        self.addref(&object.as_value());
        self.serve_call(
            closure.function,
            closure.this,
            closure.scope,
            Some(object),
            arguments,
        )
    }

    /// `zend_call_function` over a native function: [`Request::serve_method`]
    /// with no receiver and no called scope, what `Closure::__invoke`
    /// serves for a closure over a function.
    pub(crate) fn serve_function(
        &mut self,
        function: Function,
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        self.serve_call(function, None, None, None, arguments)
    }

    fn serve_call(
        &mut self,
        function: Function,
        this: Option<PhpObject>,
        called_scope: Option<ClassId>,
        closure: Option<PhpObject>,
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        let name = self.declared_name(function);
        let name = self.interned(name.as_bytes())?;
        for argument in arguments {
            self.addref(argument);
        }
        if let Some(this) = this {
            self.addref(&this.as_value());
        }
        let call = CallState {
            function,
            name: Some(name),
            arguments: arguments.to_vec(),
            count: arguments.len(),
            this,
            called_scope,
            closure,
            stage: CallStage::Serving,
            frameless: false,
            coerced: Vec::new(),
        };
        if self.user_function(function).is_some() {
            return self.serve_user(function, call);
        }
        self.vm.push_call(call);
        let served = self.serve_native(function);
        self.pop_call();
        served
    }

    /// `zend_call_function` over a user function, the branch that calls
    /// `zend_execute_ex` (design §5.6): the frame the call reached
    /// pushed as `DO_UCALL`'s `enter` pushes it, with no caller slot to
    /// return into, and the engine's loop entered on it once more,
    /// bound to the same tier as the serving that is already standing.
    /// Every frame the serving left behind is popped after it, its own
    /// last, since a throwable unwinds to here and not through the
    /// frames it passed.
    ///
    /// What the body returned is the caller's, one owner; what it left
    /// pending is the `Err`, the throwable's one owner with it, so a
    /// native reads a callback's raise the way it reads its own
    /// ([`Request::leave_pending`] puts it back).
    fn serve_user(&mut self, function: Function, call: CallState) -> Result<Value, Thrown> {
        let standing = self.vm.frames().len();
        let passed = call.arguments.len();
        let Some(frame) = self.push_user_frame(function, call, None) else {
            return self
                .fatal_of::<()>(Err(Fatal::Error(String::from(
                    "InternalError: a user function whose body the unit has not",
                ))))
                .map(|()| Value::null());
        };
        // `enter`'s rule, so a body reached from here stands where a body
        // reached from `DO_UCALL` stands: a call that filled every
        // parameter starts at the entry safepoint, past the prologue,
        // and one that did not runs the `RECV*` oplines before it.
        let parameters = self
            .function_decl(function)
            .map_or(0, |decl| decl.parameters.len());
        if passed >= parameters {
            let entry = self
                .vm
                .serving(frame)
                .and_then(|serving| {
                    serving
                        .code()
                        .and_then(|code| code.safepoints.first().copied())
                })
                .map_or(0, |at| at.0);
            self.vm.jump(frame, entry);
        }
        let reenter = self.shared.reenter;
        let served = reenter(self, frame);
        while self.vm.frames().len() > standing {
            self.pop_frame();
        }
        self.fatal_of(served)?;
        match self.pending.take() {
            Some(pending) => Err(Thrown::of(Some(pending))),
            None => Ok(std::mem::replace(&mut self.returned, Value::null())),
        }
    }

    /// The entry a `Function` names: one of the natives, or a native
    /// method numbered past them; none for a number nothing is linked
    /// under.
    fn entry_of(&self, function: Function) -> Option<crate::api::abi::Entry> {
        let at = usize::try_from(function.0).ok()?;
        match at.checked_sub(self.shared.functions.len()) {
            None => self.shared.functions.get(at).map(|(_, entry)| *entry),
            Some(method) => self.shared.methods.get(method).map(|m| m.entry),
        }
    }

    /// The native method `function` is, if it is one: its class and its
    /// index among the class's declared methods.
    fn native_method(&self, function: Function) -> Option<&crate::engine::NativeMethod> {
        let at = usize::try_from(function.0).ok()?;
        self.shared
            .methods
            .get(at.checked_sub(self.shared.functions.len())?)
    }

    /// The user function or method `function` is, if it is one: numbered
    /// past the natives and the native methods.
    pub(crate) fn user_function(&self, function: Function) -> Option<&UserFunction> {
        let at = usize::try_from(function.0).ok()?;
        self.user_functions.get(
            at.checked_sub(self.shared.functions.len())?
                .checked_sub(self.shared.methods.len())?,
        )
    }

    /// The number a user function at `index` of the table is known by.
    fn user_function_id(&self, index: usize) -> Option<Function> {
        let at = self
            .shared
            .functions
            .len()
            .checked_add(self.shared.methods.len())?
            .checked_add(index)?;
        u32::try_from(at).ok().map(Function)
    }

    /// The class a method belongs to, `func->common.scope`: a native
    /// method's class, or the request's `ClassId` for the class of a
    /// user method; none for a function.
    pub(crate) fn method_class(&self, function: Function) -> Option<ClassId> {
        if let Some(method) = self.native_method(function) {
            return Some(method.class);
        }
        let user = self.user_function(function)?;
        let exec::Body::Method { class, .. } = user.body else {
            return None;
        };
        let declared = self
            .declared
            .iter()
            .position(|(unit, at)| Arc::ptr_eq(unit, &user.unit) && *at == class)?;
        u32::try_from(self.shared.classes.len().checked_add(declared)?)
            .ok()
            .map(ClassId)
    }

    /// `zend_hash_find_ptr(&ce->function_table, lcname)` over the
    /// class's native methods: the method under the lowered name, by
    /// identity, the class's own first and then each parent's, since
    /// `zend_do_inheritance` copies a parent's table into the child's
    /// (`ReflectionObject` answers `getName` with `ReflectionClass`'s);
    /// none where no class of the chain has an entry of that name.
    pub(crate) fn method_named(&self, class: ClassId, lowered: &[u8]) -> Option<Function> {
        let mut at = Some(class);
        let mut depth = 0;
        while let Some(class) = at {
            if let Some(function) = self.own_method_named(class, lowered) {
                return Some(function);
            }
            depth += 1;
            if depth > 64 {
                return None;
            }
            at = self
                .class_decl(class)?
                .parent
                .as_ref()
                .and_then(|name| self.class_named(&name.0));
        }
        None
    }

    /// [`Request::method_named`] over one class's own declarations: a
    /// native class's among the native methods, a user class's among the
    /// user functions declared with it.
    fn own_method_named(&self, class: ClassId, lowered: &[u8]) -> Option<Function> {
        let decl = self.class_decl(class)?;
        let at = usize::try_from(class.0).ok()?;
        if let Some(declared) = at.checked_sub(self.shared.classes.len()) {
            let (unit, index) = self.declared.get(declared)?;
            let method = decl
                .methods
                .iter()
                .position(|method| method.function.name.0.eq_ignore_ascii_case(lowered))?;
            let method = u32::try_from(method).ok()?;
            let position = self.user_functions.iter().position(|user| {
                Arc::ptr_eq(&user.unit, unit)
                    && user.body
                        == exec::Body::Method {
                            class: *index,
                            method,
                        }
            })?;
            return self.user_function_id(position);
        }
        let base = self.shared.functions.len();
        self.shared
            .methods
            .iter()
            .position(|m| {
                m.class == class
                    && decl
                        .methods
                        .get(usize::try_from(m.index).unwrap_or(usize::MAX))
                        .is_some_and(|method| method.function.name.0.eq_ignore_ascii_case(lowered))
            })
            .and_then(|i| u32::try_from(base.saturating_add(i)).ok())
            .map(Function)
    }

    /// The declaration of a method, native or user, if `function` is one.
    pub(crate) fn method_decl(&self, function: Function) -> Option<&MethodDecl> {
        if let Some(method) = self.native_method(function) {
            return self
                .class_decl(method.class)?
                .methods
                .get(usize::try_from(method.index).ok()?);
        }
        let user = self.user_function(function)?;
        let exec::Body::Method { class, method } = user.body else {
            return None;
        };
        user.unit
            .classes
            .get(usize::try_from(class).ok()?)?
            .methods
            .get(usize::try_from(method).ok()?)
    }

    /// `ce->name`: the name a class was declared under, and `object`
    /// for one the tables have no declaration for.
    pub(crate) fn class_name(&self, class: ClassId) -> String {
        self.class_decl(class).map_or_else(
            || String::from("object"),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        )
    }

    /// `instanceof_function`: whether `class` is `of`, extends it, or
    /// implements it, the parents walked and each one's interfaces with
    /// it. A chain longer than any declaration can make is cut, so a
    /// declaration naming itself as its parent ends here rather than
    /// never.
    pub(crate) fn instance_of(&self, class: ClassId, of: ClassId) -> bool {
        let mut at = Some(class);
        let mut depth = 0;
        while let Some(class) = at {
            if class == of {
                return true;
            }
            let Some(decl) = self.class_decl(class) else {
                return false;
            };
            if decl
                .interfaces
                .iter()
                .filter_map(|name| self.class_named(&name.0))
                .any(|interface| depth < 64 && self.instance_of(interface, of))
            {
                return true;
            }
            depth += 1;
            if depth > 64 {
                return false;
            }
            at = decl
                .parent
                .as_ref()
                .and_then(|name| self.class_named(&name.0));
        }
        false
    }

    /// `zend_get_executed_scope`: the class of the method being served,
    /// a native method's while its entry runs and a user method's while
    /// the loop is in its body (`EX(func)->common.scope`), and none at
    /// the main op array or inside a function.
    pub(crate) fn executed_scope(&self) -> Option<ClassId> {
        self.method_class(self.executing_function()?)
    }

    /// `EX(func)` of `EG(current_execute_data)`: the native being
    /// served, or, between natives, the function whose body the
    /// innermost frame serves; none at the main op array.
    fn executing_function(&self) -> Option<Function> {
        if let Some(call) = self.vm.serving_call() {
            return Some(call.function);
        }
        self.vm.innermost()?.function
    }

    /// `ce->parent`: the linked parent of a class, none at a root.
    pub(crate) fn class_parent(&self, class: ClassId) -> Option<ClassId> {
        self.class_decl(class)?
            .parent
            .as_ref()
            .and_then(|name| self.class_named(&name.0))
    }

    /// `zend_check_protected`: whether `scope` is `class` or one of its
    /// parents, or `class` is `scope` or one of its parents; false with
    /// no scope. Each walk is cut at a depth no declaration can make.
    pub(crate) fn check_protected(&self, class: ClassId, scope: Option<ClassId>) -> bool {
        let Some(scope) = scope else {
            return false;
        };
        let up = |from: ClassId, to: ClassId| {
            let mut at = Some(from);
            for _ in 0..64 {
                match at {
                    Some(here) if here == to => return true,
                    Some(here) => at = self.class_parent(here),
                    None => return false,
                }
            }
            false
        };
        up(class, scope) || up(scope, class)
    }

    /// `zend_std_get_method`'s access check, which
    /// `zend_std_get_static_method` and `zend_std_get_constructor` share:
    /// a public method is anyone's; a private or a protected one is
    /// called from its own class's scope, a protected one from a scope
    /// `zend_check_protected` relates to it, and otherwise php's `Error`,
    /// `zend_bad_method_call`, naming the visibility, the method's class
    /// and the name as `written`, and the calling scope or `global
    /// scope`. A class with `__call` would take the call instead; none
    /// declared here has one.
    pub(crate) fn check_method_access(
        &mut self,
        function: Function,
        written: &[u8],
    ) -> Result<(), Thrown> {
        if self.method_accessible(function) {
            return Ok(());
        }
        let shown = visibility_string(self.method_visibility(function));
        let class = self
            .method_class(function)
            .map_or_else(|| String::from("object"), |class| self.class_name(class));
        let from = self.scope_phrase();
        Err(crate::api::raising::throw(
            self,
            "Error",
            &format!(
                "Call to {shown} method {class}::{}() from {from}",
                String::from_utf8_lossy(written)
            ),
        ))
    }

    /// `zend_check_method_accessible` (`Z/Zend/zend_execute.h:399`): a
    /// public method is anyone's, a private one is its own class's
    /// scope alone, and a protected one is a scope
    /// `zend_check_protected` relates to it. A function that is no
    /// method, and one the tables have no declaration for, is anyone's.
    fn method_accessible(&self, function: Function) -> bool {
        let Some(class) = self.method_class(function) else {
            return true;
        };
        let scope = self.executed_scope();
        match self.method_visibility(function) {
            Visibility::Public => true,
            _ if scope == Some(class) => true,
            Visibility::Private => false,
            Visibility::Protected => self.check_protected(class, scope),
        }
    }

    /// `fn_flags & (ZEND_ACC_PUBLIC|ZEND_ACC_PROTECTED|ZEND_ACC_PRIVATE)`:
    /// how a method was declared, public for a function that is none.
    fn method_visibility(&self, function: Function) -> Visibility {
        self.method_decl(function)
            .map_or(Visibility::Public, |decl| decl.visibility)
    }

    /// `scope ? "scope " : "global scope"` with the scope's name after
    /// it, the tail php's bad-call texts end in.
    fn scope_phrase(&self) -> String {
        match self.executed_scope() {
            Some(scope) => format!("scope {}", self.class_name(scope)),
            None => String::from("global scope"),
        }
    }

    /// `object_init_ex`'s refusals: an interface, a trait, an enum or an
    /// abstract class is php's `Error` for what cannot be instantiated.
    pub(crate) fn instantiable(&mut self, class: ClassId) -> Result<(), Thrown> {
        let Some(decl) = self.class_decl(class) else {
            return Ok(());
        };
        let what = match decl.kind {
            ClassKind::Interface => "interface",
            ClassKind::Trait => "trait",
            ClassKind::Enum => "enum",
            ClassKind::Class if decl.flags.is_abstract => "abstract class",
            ClassKind::Class => return Ok(()),
        };
        let name = self.class_name(class);
        Err(crate::api::raising::throw(
            self,
            "Error",
            &format!("Cannot instantiate {what} {name}"),
        ))
    }

    /// The name a call frame carries for a native method, the interned
    /// string of its declared name, and the empty string for the pass
    /// function, which has none.
    pub(crate) fn method_name_string(&mut self, function: Function) -> Result<PhpString, Thrown> {
        let name = if function == Function::PASS {
            String::new()
        } else {
            self.declared_name(function)
        };
        self.interned(name.as_bytes())
    }

    /// [`Request::qualified_name`] for a raise about a method called on
    /// its class.
    pub(crate) fn qualified_name_of(&self, function: Function) -> String {
        self.qualified_name(function, None)
    }

    /// An interned string of this request's heap, never refcounted, for
    /// a name the engine holds as bytes; the `Err` is the memory limit
    /// refusing the intern.
    fn interned(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        let interned = self.heap.intern(bytes);
        self.fatal_of(interned).map(PhpString::of)
    }

    /// What the entry frame of the innermost serving returned.
    pub(crate) fn set_returned(&mut self, value: Value) {
        self.returned = value;
    }

    /// Leave what a raise made pending where the loop reads it, php's
    /// `EG(exception)`. A `Thrown` that carries none is a fatal or an
    /// `exit` the raiser already recorded on the request. The
    /// throwable's one owner is the pending slot's from here; a second
    /// throwable raised while one is pending is given up, where php
    /// chains it as the first's `previous` (`zend_throw_exception_internal`),
    /// which arrives with `catch`.
    pub(crate) fn leave_pending(&mut self, thrown: &Thrown) {
        let Some(object) = thrown.exception().copied() else {
            return;
        };
        if self.pending.is_none() {
            self.pending = Some(object);
        } else {
            self.release(object.as_value());
        }
    }

    /// The pending throwable at the end of a serving, php's
    /// `zend_exception_error` (design §5.9): the throwable's own
    /// `__toString` behind `Uncaught `, with `\n  thrown` after it, given
    /// to `php_error_cb` as `E_ERROR` at the site the throwable recorded
    /// rather than the site the loop has reached. The `in ... on line ...`
    /// that closes php's last line is that call's own tail, which is why
    /// the site appears twice and in two spellings.
    ///
    /// Nothing pending means the request is ending in the fatal or the
    /// `exit` the raiser recorded, and the loop has nothing to add.
    ///
    /// ```php
    /// nope();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:2
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// The rows are the frame chain as it stands at the raise, and
    /// `{main}` is the line every trace ends on whether or not the main
    /// frame still does. A destructor the shutdown walk serves stands
    /// with that frame already popped, so it is the outermost row and
    /// php gives it no file: `[internal function]`, the site of a frame
    /// no opline called.
    ///
    /// ```php
    /// class A { function __destruct() { nope(); } }
    /// $a = new A;
    /// echo "end\n";
    /// ```
    /// ```expect exit=255
    /// end
    ///
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:2
    /// Stack trace:
    /// #0 [internal function]: A->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// A `ParseError` or a `CompileError`, the class itself and not a
    /// child, is the branch before that one: its message alone, as
    /// `E_PARSE` or `E_COMPILE_ERROR` at the site the throwable recorded,
    /// with no `Uncaught` and no trace, and the request ends as the
    /// diagnostic ends it. `token_get_all()` under `TOKEN_PARSE` is what
    /// throws one here, and its witness in `ext/tokenizer` pins the
    /// rendering; `a_parse_error_is_rendered_as_its_level` in
    /// `api::raising` holds it without the extension.
    pub(crate) fn uncaught(&mut self) -> Result<(), Fatal> {
        let Some(throwable) = self.pending.take() else {
            return Ok(());
        };
        let (file, line) = self.throw_site(throwable);
        let class = self.class_name_of(throwable);
        let compile_time = match class.as_str() {
            "ParseError" => Some(Level::Parse),
            "CompileError" => Some(Level::CompileError),
            _ => None,
        };
        let message = match compile_time {
            Some(_) => string_prop(self, throwable, b"message"),
            None => format!("Uncaught {}\n  thrown", self.throwable_to_string(throwable)),
        };
        self.end_in_uncaught(compile_time.unwrap_or(Level::Error), &message, &file, line);
        let fatal = Fatal::Error(message);
        // `zend_exception_error` ends with `OBJ_RELEASE(ex)`: the pending
        // slot's owner, given up once the rendering has read it.
        self.release(throwable.as_value());
        Err(fatal)
    }

    /// The declared name of a throwable's class, `Error` for one the
    /// tables do not know.
    fn class_name_of(&self, throwable: PhpObject) -> String {
        self.class_decl(throwable.class()).map_or_else(
            || String::from("Error"),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        )
    }

    /// `Exception::__toString` (`Z/Zend/zend_exceptions.c:692`): each
    /// throwable of the `previous` chain rendered by
    /// [`Request::rendering_of`], the deepest first and `this` last,
    /// with `\n\nNext ` between them, which is php's `prev_str` carried
    /// through the loop and put after the one it is rendering. The
    /// walk stops at a throwable it has already rendered, php's
    /// `Z_PROTECT_RECURSION_P`; a chain is a path through live objects,
    /// so the store's count is a length it cannot pass.
    ///
    /// The ` and defined` a `TypeError` whose message names a call site
    /// takes arrives with the frames that make it reachable.
    pub(crate) fn throwable_to_string(&self, throwable: PhpObject) -> String {
        let mut chain = vec![throwable];
        for _ in 0..=self.heap.objects() {
            let Some(at) = chain.last().copied() else {
                break;
            };
            let Some(previous) = self.previous_of(at) else {
                break;
            };
            if chain.iter().any(|seen| seen.index() == previous.index()) {
                break;
            }
            chain.push(previous);
        }
        chain
            .iter()
            .rev()
            .map(|throwable| self.rendering_of(*throwable))
            .collect::<Vec<String>>()
            .join("\n\nNext ")
    }

    /// One throwable of that chain: the class, the message where there
    /// is one, the site behind a colon and the trace under `Stack
    /// trace:`.
    fn rendering_of(&self, throwable: PhpObject) -> String {
        let class = self.class_name_of(throwable);
        let mut message = string_prop(self, throwable, b"message");
        // `Z/Zend/zend_exceptions.c:740`: a `TypeError` or an
        // `ArgumentCountError` whose message names the call site is
        // closed with ` and defined`, so the site the rendering puts
        // after it reads as the function's own. php compares the class
        // itself and not what it descends from.
        if (class == "TypeError" || class == "ArgumentCountError")
            && message.contains(", called in ")
        {
            message.push_str(" and defined");
        }
        let (file, line) = self.throw_site(throwable);
        let file = String::from_utf8_lossy(&file);
        let trace = self.trace_string_of(throwable);
        if message.is_empty() {
            format!("{class} in {file}:{line}\nStack trace:\n{trace}")
        } else {
            format!("{class}: {message} in {file}:{line}\nStack trace:\n{trace}")
        }
    }

    /// The `file` and `line` a throwable recorded where it was made.
    fn throw_site(&self, throwable: PhpObject) -> (Vec<u8>, u32) {
        let file = throwable
            .prop(self, b"file")
            .and_then(|value| value.as_str_bytes().map(<[u8]>::to_vec))
            .unwrap_or_default();
        let line = throwable
            .prop(self, b"line")
            .and_then(|value| value.as_int())
            .and_then(|line| u32::try_from(line).ok())
            .unwrap_or(0);
        (file, line)
    }

    /// `zend_default_exception_new`: a throwable records where it was
    /// made, `zend_get_executed_filename` and `zend_get_executed_lineno`,
    /// and the backtrace `zend_fetch_debug_backtrace` builds there, the
    /// array `getTrace` answers, written into the declared `trace` slot;
    /// `getTraceAsString`, `__toString` and the uncaught fatal render it
    /// through [`Request::trace_string_of`] when they are asked.
    pub(crate) fn note_throw_site(&mut self, throwable: PhpObject) -> Result<(), Thrown> {
        let file = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
        let trace = self.debug_backtrace()?;
        let line = Value::int(i64::from(self.current_line()));
        for (name, value) in [
            (&b"file"[..], Value::string(PhpString::new(self, &file)?)),
            (b"line", line),
            (b"trace", Value::array(trace)),
        ] {
            let name = PhpString::new(self, name)?;
            throwable.set_prop(self, name, value)?;
        }
        Ok(())
    }

    /// `zend_trace_to_string` over the `trace` a throwable holds, with
    /// `include_main`; a slot that holds no array, which is what a
    /// throwable made before this engine recorded one has, renders as
    /// the empty trace does, `#0 {main}`.
    pub(crate) fn trace_string_of(&self, throwable: PhpObject) -> String {
        match throwable
            .prop(self, b"trace")
            .and_then(|value| value.as_array().copied())
        {
            Some(trace) => self.trace_to_string(trace),
            None => String::from("#0 {main}"),
        }
    }

    /// `zend_fetch_debug_backtrace` (`Z/Zend/zend_builtin_functions.c`)
    /// with the options a throwable is built under, no `include_main`
    /// and no `DEBUG_BACKTRACE_PROVIDE_OBJECT`: one row per link of the
    /// chain below the op array being served, `file`, `line`,
    /// `function`, then `class` and `type` for a method, then `args`, in
    /// the order php adds the keys. The chain is
    /// [`Request::trace_chain`]; a row whose link stands on a native
    /// names no `file` and no `line`, which is the test php makes on
    /// `prev->func` with `ZEND_USER_CODE`.
    /// The arguments are the frame's, each followed through a reference
    /// and one more owner taken, `debug_backtrace_get_args`; under
    /// `zend.exception_ignore_args` the row has no `args`,
    /// `DEBUG_BACKTRACE_IGNORE_ARGS`. The `Err` is the memory limit
    /// refusing the rows.
    fn debug_backtrace(&mut self) -> Result<PhpArray, Thrown> {
        let ignore_arguments = self.exception_ignore_args();
        let links = self.trace_chain();
        let mut trace = PhpArray::new(self)?;
        for (function, this, scope, arguments, site) in links {
            let mut row = PhpArray::new(self)?;
            let name = self.declared_name(function);
            let method = self.method_class(function).or(scope).map(|class| {
                let class = self.class_name(class);
                (class, if this.is_some() { "->" } else { "::" })
            });
            if let Some((file, line)) = site {
                let file = Value::string(PhpString::new(self, &file)?);
                self.set_string_key(&mut row, b"file", file)?;
                self.set_string_key(&mut row, b"line", Value::int(line))?;
            }
            let name = Value::string(PhpString::new(self, name.as_bytes())?);
            self.set_string_key(&mut row, b"function", name)?;
            if let Some((class, join)) = method {
                let class = Value::string(PhpString::new(self, class.as_bytes())?);
                self.set_string_key(&mut row, b"class", class)?;
                let join = Value::string(PhpString::new(self, join.as_bytes())?);
                self.set_string_key(&mut row, b"type", join)?;
            }
            if !ignore_arguments {
                let mut args = PhpArray::new(self)?;
                for argument in arguments {
                    let argument = argument.deref();
                    self.addref(&argument);
                    args.append(self, argument)?;
                }
                self.set_string_key(&mut row, b"args", Value::array(args))?;
            }
            trace.append(self, Value::array(row))?;
        }
        Ok(trace)
    }

    /// `EG(current_execute_data)` and its `prev_execute_data` chain,
    /// innermost first: one row per link, the function it serves, its
    /// receiver, its arguments, and the site of the link below it. The
    /// main op array's frame is the link that serves no function and
    /// makes no row of its own, since `zend_trace_to_string` closes
    /// every trace with `{main}` whether or not that frame still
    /// stands; every other outermost link is a row with no site, php's
    /// `[internal function]`, which is what a shutdown function or a
    /// destructor served after the main frame popped is.
    ///
    /// php holds one chain; this engine holds two stacks, the user
    /// frames and the calls it is serving, and a frame's `calls_below`
    /// says where among the calls it was pushed, which walks them back
    /// into one. A native that re-entered the loop stands below the
    /// frame it entered, so that frame's site is the native's, which
    /// names no file: `prev->func && ZEND_USER_CODE(...)` is php's
    /// test, and a row with no `file` renders `[internal function]`.
    ///
    /// A user frame's arguments are its parameters' CVs up to the
    /// number passed and then its extra ones
    /// (`debug_backtrace_get_args`); a native's are the call's, as they
    /// were placed. A call still being built is not on the chain, as
    /// php's is a chain of activations.
    fn trace_chain(&self) -> Vec<TraceRow> {
        enum Link<'a> {
            Frame(usize, &'a exec::FrameState),
            Call(&'a exec::CallState),
        }
        let frames = self.vm.frames();
        let calls = self.vm.calls();
        let mut chain: Vec<Link> = Vec::new();
        let mut taken = 0usize;
        for (at, state) in frames.iter().enumerate() {
            let below = usize::try_from(state.calls_below)
                .unwrap_or(usize::MAX)
                .min(calls.len());
            while taken < below {
                if calls[taken].stage == exec::CallStage::Serving {
                    chain.push(Link::Call(&calls[taken]));
                }
                taken += 1;
            }
            chain.push(Link::Frame(at, state));
        }
        while taken < calls.len() {
            if calls[taken].stage == exec::CallStage::Serving {
                chain.push(Link::Call(&calls[taken]));
            }
            taken += 1;
        }
        let mut rows = Vec::new();
        for at in (0..chain.len()).rev() {
            let site = match at.checked_sub(1).map(|below| &chain[below]) {
                Some(Link::Frame(_, below)) => exec::body_of(&below.unit, below.body)
                    .map(|code| (code.filename.0.to_vec(), i64::from(exec::line_of(below)))),
                Some(Link::Call(_)) | None => None,
            };
            match &chain[at] {
                Link::Frame(place, state) => {
                    let Some(function) = state.function else {
                        continue;
                    };
                    let frame = FrameRef(u32::try_from(*place).unwrap_or(u32::MAX));
                    let passed = usize::try_from(state.num_args).unwrap_or(0);
                    let mut arguments: Vec<Value> = (0..passed
                        .saturating_sub(state.extra_args.len()))
                        .map(|slot| self.slot(frame, u32::try_from(slot).unwrap_or(u32::MAX)))
                        .collect();
                    arguments.extend(state.extra_args.iter().copied());
                    rows.push((
                        function,
                        state.this,
                        self.closure_scope(state.closure),
                        arguments,
                        site,
                    ));
                }
                Link::Call(call) => {
                    rows.push((
                        call.function,
                        call.this,
                        self.closure_scope(call.closure),
                        call.arguments.clone(),
                        site,
                    ));
                }
            }
        }
        rows
    }

    /// `ZEND_MAP_PTR_GET(closure->func.op_array.static_variables_ptr)`
    /// beside the names the op array carries: what the closure bound,
    /// name and value, in the order the table took them, with a slot
    /// nothing filled left out. Empty for an object that is no closure
    /// and for a closure whose op array binds nothing.
    pub(crate) fn closure_bound(&self, object: PhpObject) -> Vec<(Vec<u8>, Value)> {
        let Some(closure) = object.closure(self) else {
            return Vec::new();
        };
        let Some(user) = self.user_function(closure.function) else {
            return Vec::new();
        };
        let Some(code) = exec::body_of(&user.unit, user.body) else {
            return Vec::new();
        };
        code.bound_vars
            .iter()
            .zip(closure.bound.iter())
            .filter(|(_, value)| value.tag() != TypeTag::Undef)
            .map(|(name, value)| (name.0.to_vec(), *value))
            .collect()
    }

    /// `func->common.scope` of a closure's own copy of its function: the
    /// class the closure was written in, which a backtrace names before
    /// the closure's own name. None where the call stands on no closure
    /// or the closure was made outside a class.
    fn closure_scope(&self, closure: Option<PhpObject>) -> Option<ClassId> {
        closure?.closure(self)?.scope
    }

    /// `zend_hash_str_update` on a row: `value` at the string key `name`,
    /// the key a string of the request's that the array takes.
    fn set_string_key(
        &mut self,
        row: &mut PhpArray,
        name: &[u8],
        value: Value,
    ) -> Result<(), Thrown> {
        let key = Key::Str(PhpString::new(self, name)?);
        row.set(self, key, value)
    }

    /// `zend_trace_to_string` (`Z/Zend/zend_exceptions.c:668`) with
    /// `include_main`: `_build_trace_string` over each row that is an
    /// array, `#N file(line): ` where the row names a `file`, `[internal
    /// function]: ` where it names none, the `class` and `type` where
    /// there are ones, the `function`, and the `args` between the
    /// parentheses; then `#N {main}`.
    fn trace_to_string(&self, trace: PhpArray) -> String {
        let mut frames: Vec<String> = Vec::new();
        for (_, row) in &trace {
            let Some(row) = row.as_array() else {
                continue;
            };
            let field = |name: &[u8]| {
                row.into_iter()
                    .find(|(key, _)| matches!(key, Key::Str(s) if s.as_bytes() == name))
                    .map(|(_, value)| value)
            };
            let string = |name: &[u8]| {
                field(name).and_then(|v| {
                    v.as_str_bytes()
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                })
            };
            let site = match string(b"file") {
                Some(file) => {
                    let line = field(b"line").and_then(|v| v.as_int()).unwrap_or(0);
                    format!("{file}({line}): ")
                }
                None => String::from("[internal function]: "),
            };
            let class = string(b"class").unwrap_or_default();
            let join = string(b"type").unwrap_or_default();
            let function = string(b"function").unwrap_or_else(|| String::from("unknown"));
            let arguments = field(b"args")
                .and_then(|v| v.as_array().copied())
                .map(|args| {
                    let values: Vec<Value> = args.into_iter().map(|(_, v)| v).collect();
                    self.trace_arguments(&values)
                })
                .unwrap_or_default();
            frames.push(format!(
                "#{} {site}{class}{join}{function}({arguments})",
                frames.len()
            ));
        }
        let main = frames.len();
        frames.push(format!("#{main} {{main}}"));
        frames.join("\n")
    }

    /// The backtrace a fatal carries, `zend_fetch_debug_backtrace`
    /// rendered by `zend_trace_to_string` at the site, its rows given
    /// back once rendered; the memory limit refusing the rows leaves the
    /// fatal without one.
    fn trace_as_string(&mut self) -> String {
        let Ok(trace) = self.debug_backtrace() else {
            return String::from("#0 {main}");
        };
        let text = self.trace_to_string(trace);
        self.release(Value::array(trace));
        text
    }

    /// `_build_trace_args` (`Z/Zend/zend_exceptions.c:539`) over one
    /// frame's arguments, `, ` between them: `smart_str_append_zval` for a
    /// scalar and an enum case, and php's three words for the rest.
    /// `NULL`, `true`, `false` and a long's digits; a double through
    /// `smart_str_append_double` at `precision` with `zero_fraction` set,
    /// which is `zend_gcvt` and then `.0` on a finite text with no `.` in
    /// it, so `1.0`, `-0.0` and `1.0E+20` where `echo` writes `1`, `-0`
    /// and `1.0E+20`; a string in `'`, escaped by `smart_str_append_escaped`
    /// and cut at `zend.exception_string_param_max_len` bytes with `...`
    /// after the cut; `Array`, `Object(ClassName)`, `Resource id #N`, and
    /// an enum case as `Enum::Case`. A reference is followed first,
    /// `ZVAL_DEREF`.
    ///
    /// ```php
    /// memory_get_usage(null, true, false, -7, 1.0, -0.0, 1e20, 0.1, INF, NAN, "a\nb\\c\te\$\e", "exactly fifteen", "sixteen chars!!!", "é");
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: memory_get_usage() expects at most 1 argument, 14 given in %s:2
    /// Stack trace:
    /// #0 %s(2): memory_get_usage(NULL, true, false, -7, 1.0, -0.0, 1.0E+20, 0.1, INF, NAN, 'a\nb\\c\te$\e', 'exactly fifteen', 'sixteen chars!!...', '\xC3\xA9')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// `zend.exception_ignore_args` leaves the frame without its
    /// arguments, and `zend.exception_string_param_max_len` moves the cut.
    ///
    /// ```php
    /// ini_set('zend.exception_string_param_max_len', '3'); ini_set('zend.exception_ignore_args', '0');
    /// set_time_limit('seconds', 2);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: set_time_limit() expects exactly 1 argument, 2 given in %s:3
    /// Stack trace:
    /// #0 %s(3): set_time_limit('sec...', 2)
    /// #1 {main}
    ///   thrown in %s on line 3
    /// ```
    ///
    /// ```php
    /// ini_set('zend.exception_ignore_args', '1');
    /// set_time_limit('seconds', 2);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: set_time_limit() expects exactly 1 argument, 2 given in %s:3
    /// Stack trace:
    /// #0 %s(3): set_time_limit()
    /// #1 {main}
    ///   thrown in %s on line 3
    /// ```
    fn trace_arguments(&self, arguments: &[Value]) -> String {
        let mut out = String::new();
        for (i, argument) in arguments.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            self.trace_argument(&mut out, &argument.deref());
        }
        out
    }

    /// One argument as [`Request::trace_arguments`] renders it.
    fn trace_argument(&self, out: &mut String, argument: &Value) {
        match argument.as_ref() {
            ValueRef::Undef | ValueRef::Null => out.push_str("NULL"),
            ValueRef::Bool(b) => out.push_str(if b { "true" } else { "false" }),
            ValueRef::Int(n) => out.push_str(&n.to_string()),
            ValueRef::Float(f) => {
                // `smart_str_append_double`: `precision ? precision : 1`,
                // then `.0` on a finite text that carries no `.`.
                let precision = match self.precision() {
                    0 => 1,
                    precision => precision,
                };
                let text = php_heap::double_to_string(f, precision);
                out.push_str(&text);
                if f.is_finite() && !text.contains('.') {
                    out.push_str(".0");
                }
            }
            ValueRef::Str(s) => {
                out.push('\'');
                let cut = self.exception_string_param_max_len();
                append_escaped(out, &s.as_bytes()[..cut.min(s.len())]);
                if s.len() > cut {
                    out.push_str("...");
                }
                out.push('\'');
            }
            ValueRef::Array(_) => out.push_str("Array"),
            ValueRef::Object(object) => {
                let decl = self.class_decl(object.class());
                let class = decl.map_or_else(String::new, |decl| {
                    String::from_utf8_lossy(&decl.name.0).into_owned()
                });
                let case = decl
                    .filter(|decl| decl.kind == ClassKind::Enum)
                    .and_then(|_| object.prop(self, b"name"))
                    .and_then(|name| name.as_php_string().copied());
                // `zend_enum_fetch_case_name`: the case's `name`.
                if let Some(case) = case {
                    out.push_str(&class);
                    out.push_str("::");
                    out.push_str(&String::from_utf8_lossy(case.as_bytes()));
                } else {
                    out.push_str("Object(");
                    out.push_str(&class);
                    out.push(')');
                }
            }
            ValueRef::Resource(resource) => {
                out.push_str("Resource id #");
                out.push_str(&resource.id().to_string());
            }
            // `deref` followed the reference already.
            ValueRef::Reference(r) => self.trace_argument(out, &r.get()),
        }
    }

    /// `zend.exception_ignore_args`: whether a throwable's trace is
    /// built without the frames' arguments.
    fn exception_ignore_args(&self) -> bool {
        self.ini_get(b"zend.exception_ignore_args")
            .is_some_and(Request::ini_parse_bool)
    }

    /// `zend.exception_string_param_max_len`: the bytes of a string
    /// argument a trace prints before `...`, `EG(exception_string_param_max_len)`,
    /// which `OnSetExceptionStringParamMaxLen` set from the text with
    /// `ZEND_ATOL` (`Z/Zend/zend.c:177`), so `' +4x'` cuts at four.
    ///
    /// ```php
    /// var_dump(ini_set('zend.exception_string_param_max_len', ' +4x'));
    /// set_time_limit('seconds', 2);
    /// ```
    /// ```expect exit=255
    /// string(2) "15"
    ///
    /// Fatal error: Uncaught ArgumentCountError: set_time_limit() expects exactly 1 argument, 2 given in %s:3
    /// Stack trace:
    /// #0 %s(3): set_time_limit('seco...', 2)
    /// #1 {main}
    ///   thrown in %s on line 3
    /// ```
    pub(crate) fn exception_string_param_max_len(&self) -> usize {
        self.ini_get(b"zend.exception_string_param_max_len")
            .map_or(15, |value| usize::try_from(ini_atol(value)).unwrap_or(0))
    }

    /// Serve the native the call frame on top of [`Vm`] names: its entry
    /// with the request, off which it reads its arguments (ADR-0002); the
    /// frame is the active function for the raisers' text while the entry
    /// runs, and stays on the stack until `DO_ICALL` pops it. `function`
    /// is the callee the `INIT_FCALL` resolved; one with no entry is the
    /// `Error` a function nothing is linked under raises, in the caller's
    /// spelling.
    pub(crate) fn serve_native(&mut self, function: Function) -> Result<Value, Thrown> {
        if function == Function::PASS {
            return Ok(Value::null());
        }
        let Some(entry) = self.entry_of(function) else {
            let name = self
                .vm
                .serving_call()
                .and_then(|call| call.name)
                .map(|name| String::from_utf8_lossy(name.as_bytes()).into_owned())
                .unwrap_or_default();
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!("Call to undefined function {name}()"),
            ));
        };
        entry(self)
    }

    pub(crate) fn take_body(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.body)
    }

    /// The log's entries and the error stream's bytes, in the order the
    /// request wrote them, for `serve` to hand the sink one call each.
    pub(crate) fn take_aside(&mut self) -> Vec<Aside> {
        std::mem::take(&mut self.aside)
    }

    /// `php_log_err`: one entry for the host's log, whole.
    fn log_err(&mut self, entry: &[u8]) {
        self.aside.push(Aside::Log(entry.to_vec()));
    }

    /// The header block as the sink is handed it: `SG(sapi_headers).headers`
    /// rendered one line each, and nothing else, since the status line
    /// and its shape are the host's (`sapi_cgi_send_headers` writes
    /// `Status:`, the dev server the HTTP status line).
    pub(crate) fn header_block(&self) -> Vec<u8> {
        let mut block = Vec::new();
        for header in self.output.headers() {
            block.extend_from_slice(header);
            block.extend_from_slice(b"\r\n");
        }
        block
    }

    pub(crate) fn ending(&self) -> Option<&Ending> {
        self.ending.as_ref()
    }

    /// `php_error_cb`: one diagnostic, rendered twice. A repeat
    /// `ignore_repeated_errors` drops is dropped whole; what
    /// `error_reporting` masks is neither logged nor displayed; what is
    /// left goes to the log while `log_errors` is on, in php's log form,
    /// and to the body while `display_errors` is, in whichever of php's
    /// two display forms `html_errors` names, with
    /// `error_prepend_string` and `error_append_string` standing around
    /// it. The message-id table, the user handler, `EH_THROW` and the
    /// `@` scope are the rest of that branch and are not read yet.
    pub(crate) fn note_error(&mut self, level: Level, message: &str) {
        // `zend_error_va_list`'s site (`Z/Zend/zend.c:1633-1651`): the
        // executing frame's file and line, and, where nothing is
        // executing and nothing compiling, no file at all, which
        // `php_error_cb` is handed as `Unknown` at line zero
        // (`ZEND_STR_UNKNOWN_CAPITALIZED`). A destructor the shutdown
        // walk refuses to serve raises there.
        if !self.executing() {
            self.note_error_in(level, message, b"Unknown", 0);
            return;
        }
        let line = self.current_line();
        self.note_error_at(level, message, line);
    }

    /// The same diagnostic at a line no frame stands on: what a
    /// compile-time error carries, since php's `zend_error_cb` is given
    /// the compiler's line while `zend_get_executed_lineno` has none.
    pub(crate) fn note_error_at(&mut self, level: Level, message: &str, line: u32) {
        let file = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
        self.note_error_in(level, message, &file, line);
    }

    /// The same diagnostic at a file and line of its own, which is what
    /// `zend_exception_error` gives `zend_error_va` for an uncaught
    /// throwable: the site the throwable recorded, not the site the
    /// request has reached.
    ///
    /// The recipe passes both strings empty, `html_errors=0` and
    /// `ignore_repeated_errors=0`, so each witness here sets its entry
    /// with `ini_set()` first. `error_prepend_string` and
    /// `error_append_string` stand around the plain form, with no
    /// newline of their own: the form's own leading newline is what
    /// parts `<PRE>` from `Warning`.
    ///
    /// ```php
    /// ini_set('error_prepend_string', '<PRE>');
    /// ini_set('error_append_string', '</PRE>');
    /// echo NAN;
    /// ```
    /// ```expect
    /// <PRE>
    /// Warning: unexpected NAN value was coerced to string in %s on line 4
    /// </PRE>NAN
    /// ```
    ///
    /// `html_errors` is the other form, `<br />\n<b>Warning</b>:  …`,
    /// with the file and the line in `<b>` and two spaces after the
    /// colon. The message of an `E_ERROR` or an `E_PARSE` is escaped
    /// under it and a warning's stands as it is.
    ///
    /// ```php
    /// ini_set('html_errors', '1');
    /// echo NAN;
    /// ```
    /// ```expect
    /// <br />
    /// <b>Warning</b>:  unexpected NAN value was coerced to string in <b>%s</b> on line <b>3</b><br />
    /// NAN
    /// ```
    ///
    /// The `E_ERROR` an uncaught throwable becomes carries its trace in
    /// its message, and a string argument in the trace is where the
    /// four bytes `escape_html` moves reach a fatal: the argument is
    /// escaped, and the `<b>` around the file and the line are written
    /// after the message and stand.
    ///
    /// ```php
    /// ini_set('html_errors', '1');
    /// ini_set('<&">');
    /// ```
    /// ```expect exit=255
    /// <br />
    /// <b>Fatal error</b>:  Uncaught ArgumentCountError: ini_set() expects exactly 2 arguments, 1 given in %s:3
    /// Stack trace:
    /// #0 %s(3): ini_set('&lt;&amp;&quot;&gt;')
    /// #1 {main}
    ///   thrown in <b>%s</b> on line <b>3</b><br />
    /// ```
    ///
    /// `ignore_repeated_errors` drops a repeat of the last diagnostic
    /// stored, and while `ignore_repeated_source` is off a repeat is one
    /// at the same file and line: the second `NAN` on line 3 is dropped,
    /// the one on line 4 stands. With `ignore_repeated_source` on the
    /// site no longer parts them, and the one on line 6 is dropped too.
    ///
    /// ```php
    /// ini_set('ignore_repeated_errors', '1');
    /// echo NAN, NAN;
    /// echo NAN;
    /// ini_set('ignore_repeated_source', '1');
    /// echo NAN;
    /// ```
    /// ```expect
    /// Warning: unexpected NAN value was coerced to string in %s on line 3
    /// NANNAN
    /// Warning: unexpected NAN value was coerced to string in %s on line 4
    /// NANNAN
    /// ```
    ///
    /// The rest of php's display branch (`Z/main/main.c:1409-1489`),
    /// condition by condition. `E_CORE` stands past `error_reporting`,
    /// `(EG(error_reporting) & type) || (type & E_CORE)`, and the mask
    /// here reads it; nothing in this engine raises `E_CORE_ERROR` or
    /// `E_CORE_WARNING` yet, since php's own raisers of the two are its
    /// own startup and the ini loader, so the mask is held by a test
    /// on this function and no witness reaches it. `xmlrpc_errors` is
    /// the form ahead of the two `html_errors` names, read below;
    /// `PHP_INI_SYSTEM`, so a host's startup ini sets it and no
    /// `ini_set()` does, and it is held by an engine test rather than a
    /// witness, whose recipe passes the entry nothing. The stderr form
    /// is the CLI's, the CGI's and phpdbg's, `sapi_module.name` read at
    /// `Z/main/main.c:1484`; under another host `display_errors=stderr`
    /// falls to the plain form on the body, which [`Request::display_errors`]
    /// answers off the host's name. `display_startup_errors` and
    /// `during_request_startup` are not read: php's display and log
    /// branches both ask `module_initialized && !during_request_startup`
    /// before them, and a diagnostic of php's startup window is one
    /// `php_request_startup` raises between `sapi_activate` and the
    /// file's compile, in the GPC seeding or a per-directory ini. This
    /// engine raises nothing before the file's first opline, so the two
    /// entries decide nothing a request can see, and the recipe's
    /// `display_startup_errors=1` shows no difference from php's default
    /// of the same.
    ///
    /// A fatal's own trace is php 8.5's `fatal_error_backtraces`
    /// (`Z/Zend/zend.c:263`, `On` by default), the half of
    /// `zend_error_impl` before `zend_error_cb` (`Z/Zend/zend.c:1500-1526`):
    /// `EG(last_fatal_error_backtrace)` is cleared for every diagnostic,
    /// and while the entry is on a level in `E_FATAL_ERRORS` fetches one
    /// where the request stands, `zend_fetch_debug_backtrace` under
    /// `zend.exception_ignore_args`, unless a throwable is pending, which
    /// php reports ahead of the fatal instead. `php_error_cb` renders it
    /// under `Stack trace:` after the site in the log form, the xml
    /// fault, the stderr form and the plain form alike. The recipe
    /// carries `run-tests.php`'s `fatal_error_backtraces=Off`, so a
    /// witness turns the entry on first; the frames are the ones a
    /// throwable's trace walks, so a fatal at a back-edge of the main op
    /// array stands over `{main}` alone.
    ///
    /// ```php
    /// ini_set('fatal_error_backtraces', '1'); set_time_limit(1); for (;;) {}
    /// ```
    /// ```expect exit=255 wall=1s
    /// Fatal error: Maximum execution time of 1 second exceeded in %s on line 2
    /// Stack trace:
    /// #0 {main}
    /// ```
    ///
    /// A fatal raised inside a native stands over the native's frame, as
    /// a throwable's does, since both are [`Request::trace_as_string`],
    /// whose witnesses pin the frame and its arguments; the one fatal a
    /// native of this engine raises while serving, `var_dump()` over a
    /// `__debugInfo` that answers no array, waits on a compiler that
    /// reads a method, and the oracle prints
    /// `#0 %s(2): var_dump(Object(A))` over `#1 {main}` for it.
    ///
    /// The stderr form carries the trace as the plain form does, on the
    /// error stream with it.
    ///
    /// ```php
    /// ini_set('fatal_error_backtraces', '1'); ini_set('display_errors', 'stderr'); set_time_limit(1); for (;;) {}
    /// ```
    /// ```expect exit=255 wall=1s
    /// ```
    /// ```stderr
    /// Fatal error: Maximum execution time of 1 second exceeded in %s on line 2
    /// Stack trace:
    /// #0 {main}
    /// ```
    ///
    /// The trace is what parts php's two html formats
    /// (`Z/main/main.c:1471-1476`): an `E_ERROR` or an `E_PARSE` takes the
    /// first, its message escaped and no trace however the entry stands,
    /// and every other level the second, unescaped, with the trace
    /// between `<br />` and the newline that closes the line. A
    /// `TypeError` left uncaught is the `E_ERROR` an uncaught throwable
    /// becomes, and `zend_exception_error` fetches no trace for it
    /// ([`Request::end_in_uncaught`]), so under html an `E_ERROR` with a
    /// trace is the time limit's or the memory limit's, and it prints
    /// none.
    ///
    /// ```php
    /// ini_set('fatal_error_backtraces', '1'); ini_set('html_errors', '1'); set_time_limit(1); for (;;) {}
    /// ```
    /// ```expect exit=255 wall=1s
    /// <br />
    /// <b>Fatal error</b>:  Maximum execution time of 1 second exceeded in <b>%s</b> on line <b>2</b><br />
    /// ```
    ///
    /// The second html format is `E_USER_ERROR`'s, `E_COMPILE_ERROR`'s,
    /// `E_CORE_ERROR`'s and `E_RECOVERABLE_ERROR`'s; no native here
    /// raises one while serving, so
    /// `the_second_html_format_carries_the_trace_after_the_line` below
    /// holds it to the oracle's bytes for `trigger_error()`.
    pub(crate) fn note_error_in(&mut self, level: Level, message: &str, file: &[u8], line: u32) {
        // "Always clear the last backtrace."
        self.last_fatal_error_backtrace = None;
        if self.pending.is_none() && self.fatal_error_backtraces() && fatal_errors(level) {
            self.last_fatal_error_backtrace = Some(self.trace_as_string());
        }
        self.error_cb(level, message, file, line);
    }

    /// `php_error_cb` (`Z/main/main.c:1370-1500`): the diagnostic stored,
    /// masked, logged and displayed, with the trace
    /// [`Request::note_error_in`] fetched, if one, under `Stack trace:`.
    fn error_cb(&mut self, level: Level, message: &str, file: &[u8], line: u32) {
        if !self.displayed(message, file, line) {
            return;
        }
        let named = String::from_utf8_lossy(file).into_owned();
        let file = file.to_vec();
        // php stores the diagnostic before it masks it, which is why
        // `error_get_last()` answers one `error_reporting` hid.
        self.last_error = Some((level, message.to_owned(), file, line));
        // `(EG(error_reporting) & type) || (type & E_CORE)`: a core
        // level is rendered whatever `error_reporting` says.
        let core = matches!(level, Level::CoreError | Level::CoreWarning);
        if self.error_reporting() & level.bit() == 0 && !core {
            return;
        }
        let name = level_name(level);
        let trace = self.stack_trace_after_the_site();
        if self.log_errors() {
            let text = format!("PHP {name}:  {message} in {named} on line {line}{trace}");
            self.log_err(text.as_bytes());
        }
        let display = self.display_errors();
        if display == Display::Off {
            return;
        }
        if self.xmlrpc_errors() {
            // php's first display form, one `methodResponse` fault with
            // `xmlrpc_error_number` as its code and the diagnostic as
            // its string, the message unescaped and no space after the
            // colon; neither string stands around it.
            let text = format!(
                "<?xml version=\"1.0\"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>{}</int></value></member><member><name>faultString</name><value><string>{name}:{message} in {named} on line {line}{trace}</string></value></member></struct></value></fault></methodResponse>",
                self.xmlrpc_error_number()
            );
            self.write(text.as_bytes());
        } else if self.html_errors() {
            // php's two html formats: the message of an `E_ERROR` or an
            // `E_PARSE` escaped and no trace after it, and every other
            // level's as it is with the trace between the `<br />` and
            // the newline.
            let prepend = self.ini_text(b"error_prepend_string");
            let append = self.ini_text(b"error_append_string");
            let text = match level {
                Level::Error | Level::Parse => format!(
                    "{prepend}<br />\n<b>{name}</b>:  {} in <b>{named}</b> on line <b>{line}</b><br />\n{append}",
                    escape_html(message)
                ),
                _ => format!(
                    "{prepend}<br />\n<b>{name}</b>:  {message} in <b>{named}</b> on line <b>{line}</b><br />{trace}\n{append}"
                ),
            };
            self.write(text.as_bytes());
        } else if display == Display::Stderr {
            // php's stderr form stands inside the same `else` as the
            // plain one, so `html_errors` takes a CLI diagnostic off the
            // error stream and onto the body; and it carries neither
            // string around it.
            let text = format!("{name}: {message} in {named} on line {line}{trace}\n");
            self.write_stderr(text.as_bytes());
        } else {
            let text = format!(
                "{}\n{name}: {message} in {named} on line {line}{trace}\n{}",
                self.ini_text(b"error_prepend_string"),
                self.ini_text(b"error_append_string"),
            );
            self.write(text.as_bytes());
        }
    }

    /// What `php_error_cb` writes after `on line N` for the fatal's trace:
    /// `ZSTR_LEN(backtrace) ? "\nStack trace:\n" : ""` and the trace, so
    /// nothing at all while none was fetched.
    fn stack_trace_after_the_site(&self) -> String {
        self.last_fatal_error_backtrace
            .as_deref()
            .map_or_else(String::new, |trace| format!("\nStack trace:\n{trace}"))
    }

    /// `fatal_error_backtraces`, `EG(fatal_error_backtrace_on)` through
    /// `OnUpdateBool` (`Z/Zend/zend.c:263`), `PHP_INI_ALL`: whether a
    /// fatal fetches its trace. The engine's default is php's, `1`, which
    /// `fatal_error_backtraces_is_on_by_default` in `engine` holds; the
    /// recipe's `fatal_error_backtraces=Off` reaches the table through the
    /// ini scanner as the empty string, which is what php answers under
    /// the same `-d`, and `ini_set()` moves it.
    ///
    /// ```php
    /// var_dump(ini_get('fatal_error_backtraces'), ini_set('fatal_error_backtraces', '1'), ini_get('fatal_error_backtraces'));
    /// ```
    /// ```expect
    /// string(0) ""
    /// string(0) ""
    /// string(1) "1"
    /// ```
    fn fatal_error_backtraces(&self) -> bool {
        self.ini_get(b"fatal_error_backtraces")
            .is_some_and(Request::ini_parse_bool)
    }

    /// php's `display`, the first branch of `php_error_cb`: a diagnostic
    /// stands unless `ignore_repeated_errors` is on and it repeats the
    /// last one stored. It repeats when the message is the same and
    /// either `ignore_repeated_source` is on or the site is the same
    /// too. A repeat is dropped whole — not stored, not logged, not
    /// displayed — so the one it repeats stays the last error.
    fn displayed(&self, message: &str, file: &[u8], line: u32) -> bool {
        if !ini_truth(self.ini_get(b"ignore_repeated_errors")) {
            return true;
        }
        let Some((_, last, last_file, last_line)) = self.last_error.as_ref() else {
            return true;
        };
        if last != message {
            return true;
        }
        if ini_truth(self.ini_get(b"ignore_repeated_source")) {
            return false;
        }
        *last_line != line || last_file.as_slice() != file
    }

    /// `log_errors`, whether a diagnostic reaches the host's log.
    fn log_errors(&self) -> bool {
        ini_truth(self.ini_get(b"log_errors"))
    }

    /// `html_errors`, which of php's two display forms a diagnostic
    /// takes: the plain one, or `<br />\n<b>Warning</b>:  …`.
    fn html_errors(&self) -> bool {
        ini_truth(self.ini_get(b"html_errors"))
    }

    /// `xmlrpc_errors`, whether a diagnostic takes php's xml fault form
    /// ahead of both of the others.
    fn xmlrpc_errors(&self) -> bool {
        ini_truth(self.ini_get(b"xmlrpc_errors"))
    }

    /// `xmlrpc_error_number`, the fault's code, `OnUpdateLong`.
    fn xmlrpc_error_number(&self) -> i64 {
        self.ini_get(b"xmlrpc_error_number")
            .and_then(|value| String::from_utf8_lossy(value).parse().ok())
            .unwrap_or(0)
    }

    /// One ini entry as php's `STR_PRINT(INI_STR(name))` reads it: its
    /// bytes, and nothing at all where php left the entry `NULL`.
    fn ini_text(&self, name: &[u8]) -> String {
        self.ini_get(name)
            .map(|value| String::from_utf8_lossy(value).into_owned())
            .unwrap_or_default()
    }

    /// `display_errors`, php's three values: off, the body, and the
    /// host's error stream, which the hosts php names at
    /// `Z/main/main.c:1484`, `cli`, `cgi` and `phpdbg`, write a
    /// diagnostic to without php's leading newline. Under any other
    /// host `stderr` is the body, since php's test is on
    /// `sapi_module.name` and the plain form is its `else`.
    ///
    /// The value is read as `php_get_display_errors_mode`
    /// (`Z/main/main.c:492-517`) reads it: no entry, `on`, `yes`,
    /// `true` and `stdout` are `PHP_DISPLAY_ERRORS_STDOUT`, `stderr` is
    /// `PHP_DISPLAY_ERRORS_STDERR`, and any other value is `ZEND_ATOL`
    /// of it narrowed to php's `uint8_t`, where `2` is the error stream,
    /// `0` is off and every other number is `STDOUT`. The narrowing is
    /// php's and observable, `256` is off and `258` the error stream,
    /// so it is copied.
    fn display_errors(&self) -> Display {
        let mode = match self.ini_get(b"display_errors") {
            None => DISPLAY_ERRORS_STDOUT,
            Some(value) => match value.to_ascii_lowercase().as_slice() {
                b"on" | b"yes" | b"true" | b"stdout" => DISPLAY_ERRORS_STDOUT,
                b"stderr" => DISPLAY_ERRORS_STDERR,
                _ => display_errors_mode(ini_atol(value)),
            },
        };
        match mode {
            0 => Display::Off,
            DISPLAY_ERRORS_STDERR if self.host_writes_stderr() => Display::Stderr,
            _ => Display::Body,
        }
    }

    /// Whether the host is one php writes `display_errors=stderr` to
    /// the error stream under.
    fn host_writes_stderr(&self) -> bool {
        matches!(
            self.description.sapi_name.as_str(),
            "cli" | "cgi" | "phpdbg"
        )
    }

    /// `zend_error_noreturn(E_ERROR, ...)`: `php_error_cb` renders the
    /// fatal where the request stands, on the streams the two ini entries
    /// say, and the request ends in it. It is rendered once, here, and
    /// `serve` writes nothing of its own.
    pub(crate) fn end_in_fatal(&mut self, message: &str) {
        // The first fatal is the one that ends the request, since php
        // leaves the request from the first and renders no second.
        if self.ending.is_some() {
            return;
        }
        let file = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
        let line = self.current_line();
        self.note_error_in(Level::Error, message, &file, line);
        self.end_at_level(Level::Error, message, &file, line);
    }

    /// `zend_error_noreturn(E_CORE_ERROR, ...)`: the one core fatal
    /// this engine raises, `zend_objects_destroy_object`'s. php gives
    /// `E_CORE_ERROR` no site at all (`get_filename_lineno`,
    /// `Z/Zend/zend.c:1642`), so it is rendered at `Unknown` on line
    /// zero wherever it was raised, and the request ends in it.
    fn end_in_core_error(&mut self, message: &str) {
        if self.ending.is_some() {
            return;
        }
        self.note_error_in(Level::CoreError, message, b"Unknown", 0);
        self.end_at_level(Level::CoreError, message, b"Unknown", 0);
    }

    /// `zend_exception_error` (`Z/Zend/zend_exceptions.c:936-990`): an
    /// uncaught throwable rendered at the site it recorded, through
    /// `zend_error_cb` alone and never `zend_error_impl`, with
    /// `EG(last_fatal_error_backtrace)` cleared first, so the diagnostic
    /// carries the throwable's own trace in its message and never a
    /// fatal's after the site, whatever `fatal_error_backtraces` says. A
    /// `ParseError` or a `CompileError` is rendered at its own level and
    /// the rest as `E_ERROR`, and the request ends as the level ends it.
    pub(crate) fn end_in_uncaught(&mut self, level: Level, message: &str, file: &[u8], line: u32) {
        if self.ending.is_some() {
            return;
        }
        self.last_fatal_error_backtrace = None;
        self.error_cb(level, message, file, line);
        self.end_at_level(level, message, file, line);
    }

    /// The ending alone, the diagnostic already rendered at its own
    /// level: php's status alone for `E_PARSE`, and for every other
    /// fatal level the log form the host is handed, the trace under
    /// `Stack trace:` where `php_error_cb` gave `php_log_err` one.
    fn end_at_level(&mut self, level: Level, message: &str, file: &[u8], line: u32) {
        if self.ending.is_some() {
            return;
        }
        self.ending = Some(match level {
            Level::Parse => Ending::Parse,
            _ => Ending::Fatal(format!(
                "PHP Fatal error:  {message} in {} on line {line}{}",
                String::from_utf8_lossy(file),
                self.stack_trace_after_the_site()
            )),
        });
    }

    /// `E_PARSE` or `E_COMPILE_ERROR` at a site of its own, which is what
    /// the compiler's refusal carries: rendered once through
    /// `php_error_cb` at its level, and the request ended as that level
    /// ends it, php's status alone for the parse error and the log form
    /// beside it for the compile error, which `php_error_cb` bails out
    /// of as it does from `E_ERROR`. php's parser refuses by throwing a
    /// `ParseError` (`zenderror`, `Z/Zend/zend_language_parser.y`), which
    /// `zend_exception_error` renders with no trace fetched, while the
    /// compiler's `zend_error_noreturn(E_COMPILE_ERROR, …)` goes through
    /// `zend_error_impl` and fetches one, `#0 {main}` with no frame
    /// standing.
    pub(crate) fn end_in_compile_time(
        &mut self,
        level: Level,
        message: &str,
        file: &[u8],
        line: u32,
    ) {
        if level == Level::Parse {
            self.end_in_uncaught(level, message, file, line);
            return;
        }
        self.note_error_in(level, message, file, line);
        self.end_at_level(level, message, file, line);
    }

    pub(crate) fn end_in_exit(&mut self, code: i32) {
        if self.ending.is_none() {
            self.ending = Some(Ending::Exit(code));
        }
    }

    pub(crate) fn end_in(&mut self, fatal: &Fatal) {
        match fatal {
            Fatal::Error(message) => self.end_in_fatal(message),
            Fatal::OutOfMemory(bytes) => self.end_in_fatal(&format!(
                "Allowed memory size of {} bytes exhausted (tried to allocate {bytes} bytes)",
                self.ini_parse_bytes_of("memory_limit")
            )),
        }
    }

    /// `zend_objects_new` over `class` (`object_init_ex` without the
    /// constructor): a store entry out of this request's heap with one
    /// owner, a slot per declared instance property, as
    /// `zend_object_properties_size` counts them, and the memory limit
    /// as the fatal that ends the request.
    pub(crate) fn new_object(&mut self, class: ClassId) -> Result<PhpObject, Thrown> {
        let slots = self.class_slots(class);
        // `ce->create_object`: a class that declares a native payload
        // lays it down here, with the object, as
        // `zend_object_alloc(sizeof(custom), ce)` does; every other
        // class is `zend_objects_new` (design §5.4).
        let allocated = match self.native_new_of(class) {
            Some(new) => new(&mut self.heap, class.0, slots),
            None => self.heap.object(class.0, slots),
        };
        let object = self.fatal_of(allocated).map(PhpObject::of)?;
        if let Err(thrown) = self.init_properties(object) {
            self.release(object.as_value());
            return Err(thrown);
        }
        // `zend_default_exception_new`, the throwables' `create_object`:
        // `file`, `line` and `trace` are the site of `new`, written as
        // the object is made and before any constructor runs.
        if crate::classes::records_throw_site(self.handlers_of(class))
            && let Err(thrown) = self.note_throw_site(object)
        {
            self.release(object.as_value());
            return Err(thrown);
        }
        Ok(object)
    }

    /// [`Request::new_object`] carrying `payload` as the native slot of
    /// design §5.4, `zend_object_alloc(sizeof(custom), ce)`: what an
    /// internal class's methods reach through the receiver as `T`, and
    /// what its table's `free_obj` and `get_gc` release and walk.
    pub(crate) fn new_object_with<T: std::any::Any + Send>(
        &mut self,
        class: ClassId,
        payload: T,
    ) -> Result<PhpObject, Thrown> {
        let slots = self.class_slots(class);
        let allocated = self.heap.object_with(class.0, slots, payload);
        let object = self.fatal_of(allocated).map(PhpObject::of)?;
        if let Err(thrown) = self.init_properties(object) {
            self.release(object.as_value());
            return Err(thrown);
        }
        Ok(object)
    }

    /// `object_properties_init`: each declared instance property's slot
    /// takes the class's default, `zend_update_class_constants` having
    /// evaluated the expression it was written as; a property with no
    /// default is `null` where it is untyped and stays uninitialised
    /// where it is typed, as `zend_declare_typed_property` leaves it.
    fn init_properties(&mut self, object: PhpObject) -> Result<(), Thrown> {
        let class = object.class();
        let defaults: Vec<(bool, Option<php_unit::decl::ConstExpr>)> = self
            .class_decl(class)
            .map(|decl| {
                decl.properties
                    .iter()
                    .filter(|p| p.is_instance)
                    .map(|p| (p.ty.is_some(), p.default.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (i, (typed, default)) in defaults.into_iter().enumerate() {
            let value = match default {
                Some(expr) => crate::api::call::eval_const_expr(self, &expr, Some(class))?,
                None if typed => continue,
                None => Value::null(),
            };
            let slot = u32::try_from(i).unwrap_or(u32::MAX);
            Heap::object_set_slot(&mut Hooks(self), object.heap(), slot, value.into_zval());
        }
        Ok(())
    }

    /// [`Request::new_object`] by the class's name; a name the tables do
    /// not know is class zero, `Stringable`, as the skeleton has it.
    pub(crate) fn new_object_of(&mut self, class_name: &str) -> Result<PhpObject, Thrown> {
        let class = self
            .class_named(class_name.as_bytes())
            .unwrap_or(ClassId(0));
        self.new_object(class)
    }

    /// `ce->default_properties_count`: the declared instance properties,
    /// each a slot on the object; the linker that places an inherited
    /// one arrives with the classes of design §11 days 8–14.
    fn class_slots(&self, class: ClassId) -> u32 {
        self.class_decl(class).map_or(0, |decl| {
            u32::try_from(decl.properties.iter().filter(|p| p.is_instance).count())
                .unwrap_or(u32::MAX)
        })
    }

    /// The slot a declared instance property of `class` sits in,
    /// `zend_get_property_offset` without visibility yet; `None` for a
    /// name the class did not declare, which is a dynamic property.
    fn prop_slot(&self, class: ClassId, name: &[u8]) -> Option<u32> {
        let decl = self.class_decl(class)?;
        decl.properties
            .iter()
            .filter(|p| p.is_instance)
            .position(|p| &*p.name.0 == name)
            .and_then(|i| u32::try_from(i).ok())
    }

    /// A property read, `zend_std_read_property` without the handler
    /// in the way: the declared slot, `None` while nothing wrote it, or
    /// the dynamic table's entry. A view, no owner taken.
    pub(crate) fn object_prop(&self, o: Obj, name: &[u8]) -> Option<Value> {
        if let Some(i) = self.prop_slot(ClassId(o.class()), name) {
            let z = o.slot(i);
            return (!z.is_undef()).then(|| Value::of_zval(z));
        }
        o.properties()?
            .iter()
            .find(|(k, _)| matches!(k, php_heap::Key::Str(s) if s.bytes() == name))
            .map(|(_, z)| Value::of_zval(z))
    }

    /// `zend_get_property_offset` over the class's declared instance
    /// properties: the slot `name` sits in, its visibility, its set
    /// visibility and whether it is typed; none for a name the class
    /// did not declare, which is a dynamic property.
    fn declared_slot(&self, class: ClassId, name: &[u8]) -> Option<DeclaredSlot> {
        self.class_decl(class).and_then(|decl| {
            decl.properties
                .iter()
                .filter(|p| p.is_instance)
                .enumerate()
                .find(|(_, p)| &*p.name.0 == name)
                .map(|(i, p)| DeclaredSlot {
                    slot: u32::try_from(i).unwrap_or(u32::MAX),
                    visibility: p.visibility,
                    set_visibility: p.set_visibility,
                    typed: p.ty.is_some(),
                })
        })
    }

    /// `zend_asymmetric_property_has_set_access`
    /// (`Z/Zend/zend_object_handlers.c:590`): whether the executed
    /// scope may write a property of `class` whose set visibility is
    /// `set`. The declaring class writes its own; a `protected(set)`
    /// one is a related scope's too. A `public(set)` never reaches
    /// here, since `zend_declare_typed_property` drops a set
    /// visibility the read visibility already equals and refuses one
    /// weaker than it.
    fn has_set_access(&self, class: ClassId, set: Visibility) -> bool {
        let scope = self.executed_scope();
        scope == Some(class) || (set == Visibility::Protected && self.check_protected(class, scope))
    }

    /// `zend_asymmetric_visibility_property_modification_error`
    /// (`Z/Zend/zend_execute.c:931`): php's `Error` for a write the set
    /// visibility refuses, `operation` the word the site does
    /// (`modify`, `indirectly modify`, `unset`). php names the called
    /// scope here, not the executed one; the two differ only where a
    /// method is reached through a class that inherited it, and the
    /// linker that inherits a method arrives with the classes of design
    /// §11 days 8-14.
    fn set_inaccessible_property(
        &mut self,
        class: ClassId,
        set: Visibility,
        name: &[u8],
        operation: &str,
    ) -> Thrown {
        let word = match set {
            Visibility::Private => "private(set)",
            _ => "protected(set)",
        };
        let from = match self.executed_scope() {
            Some(scope) => format!("scope {}", self.class_name(scope)),
            None => String::from("global scope"),
        };
        let class = self.class_name(class);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw(
            self,
            "Error",
            &format!("Cannot {operation} {word} property {class}::${shown} from {from}"),
        )
    }

    /// `zend_verify_property_access`: whether the executed scope may
    /// reach a property of `class` declared with `visibility`.
    fn may_access(&self, class: ClassId, visibility: Visibility) -> bool {
        let scope = self.executed_scope();
        match visibility {
            Visibility::Public => true,
            _ if scope == Some(class) => true,
            Visibility::Private => false,
            Visibility::Protected => self.check_protected(class, scope),
        }
    }

    /// `CE_CONSTANTS_TABLE(ce)` as `zend_do_inheritance` leaves it, read
    /// by walking the declarations instead of copying them: the class's
    /// own constants, then its parent's and its interfaces', and a
    /// parent's private constant left out, which is
    /// `do_inherit_class_constant`'s skip. The answer is the class that
    /// declared it, which is the scope the expression is evaluated in
    /// and the scope its visibility is checked against, and the
    /// declaration itself.
    fn class_constant_of(
        &self,
        class: ClassId,
        name: &[u8],
        depth: u32,
    ) -> Option<(ClassId, ClassConstDecl)> {
        if depth > 64 {
            return None;
        }
        let decl = self.class_decl(class)?;
        if let Some(constant) = decl
            .constants
            .iter()
            .find(|c| *c.name.0 == *name && (depth == 0 || c.visibility != Visibility::Private))
        {
            return Some((class, constant.clone()));
        }
        let sources: Vec<Name> = decl
            .parent
            .iter()
            .chain(decl.interfaces.iter())
            .cloned()
            .collect();
        for source in sources {
            let Some(source) = self.class_named(&source.0) else {
                continue;
            };
            if let Some(found) = self.class_constant_of(source, name, depth.saturating_add(1)) {
                return Some(found);
            }
        }
        None
    }

    /// `zend_fetch_class_constant`'s lookup (`Z/Zend/zend_constants.c:355`):
    /// the constant `name` of `class` from the declaration's table,
    /// `Undefined constant C::A` where it has none, `Cannot access
    /// private constant C::A` where `zend_verify_const_access` refuses
    /// the executing scope, and otherwise its value, the constant
    /// expression the declaration kept evaluated in the class's scope
    /// (`zval_update_constant_ex`), one owner the caller's.
    pub(crate) fn class_constant(&mut self, class: ClassId, name: &[u8]) -> Result<Value, Thrown> {
        let class_name = self.class_name(class);
        let shown = String::from_utf8_lossy(name).into_owned();
        let Some((declaring, constant)) = self.class_constant_of(class, name, 0) else {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!("Undefined constant {class_name}::{shown}"),
            ));
        };
        let visibility = constant.visibility;
        let value = constant.value;
        let class = declaring;
        if !self.may_access(class, visibility) {
            let word = match visibility {
                Visibility::Public => "public",
                Visibility::Protected => "protected",
                Visibility::Private => "private",
            };
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!("Cannot access {word} constant {class_name}::{shown}"),
            ));
        }
        // `zend_fetch_class_constant`'s `ZEND_CLASS_CONST_IS_CASE` arm:
        // a constant of an enum is a case, and a case is an instance of
        // the enum and not the expression it was written with.
        if self
            .class_decl(class)
            .is_some_and(|decl| decl.kind == ClassKind::Enum)
        {
            return self.enum_case(class, name).map(|case| case.as_value());
        }
        crate::api::call::eval_const_expr(self, &value, Some(class))
    }

    /// `zend_enum_get_case` (`Z/Zend/zend_enum.c:410`): the one instance
    /// of `class` standing for the case `name`, made on the first read
    /// and handed back by identity from then on, its `name` the case's
    /// own and, for a backed enum, its `value` the backing value the
    /// declaration carries. One more owner is taken for the caller, the
    /// slot keeping its own, as php's constant table keeps one.
    pub(crate) fn enum_case(&mut self, class: ClassId, name: &[u8]) -> Result<PhpObject, Thrown> {
        if let Some((_, case)) = self
            .enum_cases
            .iter()
            .find(|((held, under), _)| *held == class && under == name)
        {
            let case = *case;
            self.addref(&case.as_value());
            return Ok(case);
        }
        let Some((_, constant)) = self.class_constant_of(class, name, 0) else {
            let class = self.class_name(class);
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!(
                    "Undefined constant {class}::{}",
                    String::from_utf8_lossy(name)
                ),
            ));
        };
        let backing = self
            .class_decl(class)
            .map_or(php_unit::decl::EnumBacking::Pure, |decl| decl.backing);
        let case = self.new_object(class)?;
        let spelled = PhpString::new(self, name)?;
        let slot = PhpString::new(self, b"name")?;
        case.set_prop(self, slot, Value::string(spelled))?;
        if backing != php_unit::decl::EnumBacking::Pure {
            let value = crate::api::call::eval_const_expr(self, &constant.value, Some(class))?;
            let slot = PhpString::new(self, b"value")?;
            case.set_prop(self, slot, value)?;
        }
        self.enum_cases.push(((class, name.to_vec()), case));
        self.addref(&case.as_value());
        Ok(case)
    }

    /// `zend_bad_property_access`: php's `Error` for a property the
    /// scope may not reach.
    fn inaccessible_property(
        &mut self,
        class: ClassId,
        visibility: Visibility,
        name: &[u8],
    ) -> Thrown {
        let word = match visibility {
            Visibility::Public => "public",
            Visibility::Protected => "protected",
            Visibility::Private => "private",
        };
        let class = self.class_name(class);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw(
            self,
            "Error",
            &format!("Cannot access {word} property {class}::${shown}"),
        )
    }

    /// `zend_std_read_property` under `BP_VAR_R`, the handler
    /// `FETCH_OBJ_R` calls: `zend_get_property_offset` places the name,
    /// a declared property's slot where the scope may see it, php's
    /// `Error` `Cannot access private property C::$p` where it may not
    /// (`zend_bad_property_access`), and the dynamic table for a name
    /// the class did not declare. A typed slot nothing has written is
    /// php's `Error` for a property read before initialization; an
    /// untyped one that `unset` emptied reads as a dynamic name does,
    /// and a name neither holds warns `Undefined property: C::$p` and
    /// is `null`. What is read is one more owner of the value
    /// (`ZVAL_COPY_DEREF`), a reference followed. A `__get` would be
    /// served between the slot and the warning; no class here has one.
    pub(crate) fn read_property(
        &mut self,
        object: PhpObject,
        name: &[u8],
    ) -> Result<Value, Thrown> {
        let o = object.heap();
        let class = object.class();
        let shown = String::from_utf8_lossy(name).into_owned();
        if let Some(placed) = self.declared_slot(class, name) {
            if !self.may_access(class, placed.visibility) {
                return Err(self.inaccessible_property(class, placed.visibility, name));
            }
            let z = o.slot(placed.slot);
            if !z.is_undef() {
                let value = Value::of_zval(z).deref();
                self.addref(&value);
                return Ok(value);
            }
            if placed.typed {
                let class = self.class_name(class);
                return Err(crate::api::raising::throw(
                    self,
                    "Error",
                    &format!(
                        "Typed property {class}::${shown} must not be accessed before initialization"
                    ),
                ));
            }
        }
        let dynamic = o
            .properties()
            .into_iter()
            .flat_map(|table| table.iter().collect::<Vec<_>>())
            .find(|(k, _)| matches!(k, php_heap::Key::Str(s) if s.bytes() == name))
            .map(|(_, z)| Value::of_zval(z));
        if let Some(value) = dynamic {
            let value = value.deref();
            self.addref(&value);
            return Ok(value);
        }
        let class = self.class_name(class);
        crate::api::raising::warning(self, &format!("Undefined property: {class}::${shown}"));
        Ok(Value::null())
    }

    /// `zend_get_property_offset` at a write site, what `FETCH_OBJ_W`
    /// and `ASSIGN_OBJ` place the name by: the declared slot `name` sits
    /// in and whether it is typed, where the executed scope may reach
    /// it; php's `Error` `Cannot access private property C::$p` where
    /// it may not (`zend_bad_property_access`); and none for a name the
    /// class did not declare, which is a dynamic property. A property
    /// with a set visibility is held to that one as well
    /// ([`Request::has_set_access`]), which is the second check
    /// `zend_std_write_property` and `zend_std_get_property_ptr_ptr`
    /// make; `operation` is the word the site's error carries.
    ///
    /// ```php
    /// class Foo { public private(set) int $bar = 1; public protected(set) int $baz = 2; }
    /// $f = new Foo;
    /// try { $f->bar = 3; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $f->baz = 4; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $f->bar->x = 5; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { unset($f->bar); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($f->bar, $f->baz);
    /// ```
    /// ```expect
    /// Cannot modify private(set) property Foo::$bar from global scope
    /// Cannot modify protected(set) property Foo::$baz from global scope
    /// Cannot indirectly modify private(set) property Foo::$bar from global scope
    /// Cannot unset private(set) property Foo::$bar from global scope
    /// int(1)
    /// int(2)
    /// ```
    ///
    /// Uncaught, the write from global scope is php's `Error`:
    ///
    /// ```php
    /// class Foo { public private(set) int $bar = 1; }
    /// $f = new Foo;
    /// $f->bar = 2;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Cannot modify private(set) property Foo::$bar from global scope in %s:4
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 4
    /// ```
    ///
    /// A property the class declares with a set visibility and no
    /// default is uninitialised like any other typed one:
    ///
    /// ```php
    /// class Foo { public private(set) int $bar; }
    /// $f = new Foo;
    /// var_dump($f->bar);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Typed property Foo::$bar must not be accessed before initialization in %s:4
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 4
    /// ```
    pub(crate) fn declared_slot_for_write(
        &mut self,
        class: ClassId,
        name: &[u8],
        operation: &str,
    ) -> Result<Option<(u32, bool)>, Thrown> {
        let Some(placed) = self.declared_slot(class, name) else {
            return Ok(None);
        };
        if !self.may_access(class, placed.visibility) {
            return Err(self.inaccessible_property(class, placed.visibility, name));
        }
        if let Some(set) = placed.set_visibility
            && !self.has_set_access(class, set)
        {
            return Err(self.set_inaccessible_property(class, set, name, operation));
        }
        Ok(Some((placed.slot, placed.typed)))
    }

    /// `zend_assign_to_variable` over a declared slot, `ASSIGN_OBJ`'s
    /// fast path over a slot that holds a value (`fast_assign_obj`):
    /// `value` is the slot's from here and what it held is released
    /// last, after the write, which is where a destructor is served; a
    /// slot holding a reference is written through the cell
    /// (`Z_ISREF_P(variable_ptr)`), so every holder of the cell reads
    /// the new value.
    pub(crate) fn assign_declared_slot(&mut self, o: Obj, i: u32, value: Value) {
        if let Some(r) = o.slot(i).as_reference() {
            self.reference_set(r, value);
        } else {
            Heap::object_set_slot(&mut Hooks(self), o, i, value.into_zval());
        }
    }

    /// `OBJ_PROP_NUM(obj, i)` written with no name in hand, what a
    /// native that laid the object down does (`add_token`): the slot is
    /// `value`'s from here and what it held is released last, as
    /// [`Heap::object_set_slot`] does. An index past the declared count
    /// is the internal-error fatal naming the class and the index, the
    /// value released, since no slot is there to take it.
    pub(crate) fn write_declared_slot(
        &mut self,
        o: Obj,
        i: u32,
        value: Value,
    ) -> Result<(), Thrown> {
        if i >= o.slots() {
            self.release(value);
            let class = self.class_name(ClassId(o.class()));
            let slots = o.slots();
            return self.fatal_of(Err(Fatal::Error(format!(
                "InternalError: slot {i} written on an object of class {class}, which declares {slots}"
            ))));
        }
        Heap::object_set_slot(&mut Hooks(self), o, i, value.into_zval());
        Ok(())
    }

    /// `zend_std_write_property`, the `Std` slot: into the declared
    /// slot, what it held released last, or into the dynamic table,
    /// materialised on the first write and separated when a cast shares
    /// it. The name is given up once the table has taken its own owner
    /// of it. A name the table does not hold yet is a creation and goes
    /// through [`Request::dynamic_property_creation`] first, php's two
    /// arms at `write_std_property`'s `else`; the value is given up with
    /// the `Error`, as `FREE_OP_DATA` gives it up. The typed-property
    /// coercion arrives with the kernel that writes one.
    pub(crate) fn write_property(
        &mut self,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        let o = object.heap();
        if let Some(i) = self.prop_slot(object.class(), name.as_bytes()) {
            let z = value.into_zval();
            Heap::object_set_slot(&mut Hooks(self), o, i, z);
        } else {
            let key = php_heap::Key::Str(name.heap());
            let held = o.properties().is_some_and(|table| table.contains(key));
            if !held
                && let Err(thrown) = self.dynamic_property_creation(object.class(), name.as_bytes())
            {
                self.release(value);
                self.release(Value::string(name));
                return Err(thrown);
            }
            let table = self.heap.object_properties(o);
            let table = self.fatal_of(table)?;
            let table = self.array_separate(table)?;
            self.heap.object_set_properties(o, table);
            self.array_update(table, key, value)?;
        }
        self.release(Value::string(name));
        Ok(())
    }

    /// The two arms php takes before it lays a name the class did not
    /// declare into the dynamic table, in
    /// `zend_std_write_property` and in `zend_std_get_property_ptr_ptr`
    /// alike (`Z/Zend/zend_object_handlers.c:1288`, `:1522`): a class
    /// with `ZEND_ACC_NO_DYNAMIC_PROPERTIES` is
    /// `zend_forbidden_dynamic_property`'s `Error`, and a class without
    /// `ZEND_ACC_ALLOW_DYNAMIC_PROPERTIES` is
    /// `zend_deprecated_dynamic_property`'s 8.2 deprecation. Only a
    /// creation takes them: a name the table already holds is written
    /// with neither.
    ///
    /// php's raiser holds one more owner of the object across the
    /// deprecation and turns the last release into the same `Error`;
    /// nothing here can drop the object under a diagnostic, since the
    /// handler that reached this holds it.
    pub(crate) fn dynamic_property_creation(
        &mut self,
        class: ClassId,
        name: &[u8],
    ) -> Result<(), Thrown> {
        use php_unit::decl::DynamicProperties;
        let Some(decl) = self.class_decl(class) else {
            return Ok(());
        };
        let carries = decl.flags.dynamic_properties;
        if carries == DynamicProperties::Allowed || self.ancestor_declares_property(class, name) {
            return Ok(());
        }
        let shown = String::from_utf8_lossy(name).into_owned();
        let class_name = self.class_name(class);
        if carries == DynamicProperties::Forbidden {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!("Cannot create dynamic property {class_name}::${shown}"),
            ));
        }
        crate::api::raising::deprecated(
            self,
            &format!("Creation of dynamic property {class_name}::${shown} is deprecated"),
        );
        Ok(())
    }

    /// Whether a class above `class` declares `name` as an instance
    /// property. php has no such question: `zend_get_property_offset`
    /// finds an inherited property at the slot the linker placed it in,
    /// so a write to it is never a creation. This engine's linker does
    /// not place inherited members yet (#419), so a throwable subclass
    /// carries no slot for `message`, `file`, `line` or `trace` and the
    /// raisers of `api::raising` write them into the dynamic table.
    /// They are properties php declares, so none of them is a creation
    /// and none of them is deprecated; the walk is this statement, and
    /// it goes when the slots are placed.
    fn ancestor_declares_property(&self, class: ClassId, name: &[u8]) -> bool {
        let mut at = self
            .class_decl(class)
            .and_then(|decl| decl.parent.as_ref())
            .and_then(|parent| self.class_named(&parent.0));
        let mut depth = 0;
        while let Some(class) = at {
            let Some(decl) = self.class_decl(class) else {
                return false;
            };
            if decl
                .properties
                .iter()
                .any(|p| p.is_instance && &*p.name.0 == name)
            {
                return true;
            }
            depth += 1;
            if depth > 64 {
                return false;
            }
            at = decl
                .parent
                .as_ref()
                .and_then(|parent| self.class_named(&parent.0));
        }
        false
    }

    /// `ZEND_FE_RESET_R`'s separation of a shared property table
    /// (`Z/Zend/zend_vm_def.h:6903`): the loop walks the object's own
    /// table, so one a cast shares is duplicated first and the object
    /// takes the copy, php's `zend_array_dup` under
    /// `GC_REFCOUNT(zobj->properties) > 1`. An object that has no table
    /// has nothing to separate and is given none, as php's arm is under
    /// `Z_OBJ_P(array_ptr)->properties` being there at all.
    pub(crate) fn separate_object_properties(&mut self, object: PhpObject) -> Result<(), Thrown> {
        let o = object.heap();
        let Some(table) = o.properties() else {
            return Ok(());
        };
        let table = self.array_separate(table)?;
        self.heap.object_set_properties(o, table);
        Ok(())
    }

    /// `zend_fe_fetch_object_helper` (`Z/Zend/zend_vm_def.h:7315`) over
    /// an object whose class has no `get_iterator`: the first property
    /// at or past `from` the executing scope may see, with the position
    /// it sits at, its key and a view of its value. The order is
    /// `Z_OBJPROP`'s, which is [`Request::object_entries`]' order: the
    /// class's declared instance properties in declaration order, then
    /// the dynamic table. A slot nothing has written is stepped over as
    /// php steps over an `IS_UNDEF` bucket, and one the executing scope
    /// may not reach is stepped over as php steps over a mangled key
    /// `zend_check_property_access` refuses. `None` is nothing left,
    /// which is the fetch's jump past the loop. The `Err` is the memory
    /// limit refusing the intern of a declared name; php's key is
    /// interned already and takes no allocation.
    pub(crate) fn object_property_from(
        &mut self,
        object: PhpObject,
        from: u32,
    ) -> Result<Option<(u32, php_heap::Key, Value)>, Thrown> {
        let o = object.heap();
        let class = object.class();
        let declared: Vec<(Box<[u8]>, Visibility)> = self
            .class_decl(class)
            .map(|decl| {
                decl.properties
                    .iter()
                    .filter(|p| p.is_instance)
                    .map(|p| (p.name.0.clone(), p.visibility))
                    .collect()
            })
            .unwrap_or_default();
        let mut at = from as usize;
        while let Some((name, visibility)) = declared.get(at) {
            let z = o.slot(u32::try_from(at).unwrap_or(u32::MAX));
            if !z.is_undef() && self.may_access(class, *visibility) {
                let interned = self.heap.intern(name);
                let key = self.fatal_of(interned).map(php_heap::Key::Str)?;
                let at = u32::try_from(at).unwrap_or(u32::MAX);
                return Ok(Some((at, key, Value::of_zval(z))));
            }
            at = at.saturating_add(1);
        }
        let Some(table) = o.properties() else {
            return Ok(None);
        };
        let skipped = at.saturating_sub(declared.len());
        let Some((key, z)) = (&table).into_iter().nth(skipped) else {
            return Ok(None);
        };
        let at = u32::try_from(declared.len().saturating_add(skipped)).unwrap_or(u32::MAX);
        Ok(Some((at, key, Value::of_zval(z))))
    }

    /// `zend_std_get_property_ptr_ptr`'s dynamic arm under `BP_VAR_W`
    /// (`Z/Zend/zend_object_handlers.c:1491`): the dynamic table
    /// separated where a cast shares it and the name looked up in it;
    /// where it is not there, php's two arms over the creation
    /// ([`Request::dynamic_property_creation`]) and then
    /// `zend_hash_add(zobj->properties, name, &EG(uninitialized_zval))`,
    /// which is `IS_NULL` (`zend_execute_API.c:132`) and is what the
    /// `ASSIGN*` beside this writes through, and what a caught `Error`
    /// leaves the property holding. The name is given up here, as
    /// [`Request::write_property`] gives it up.
    ///
    /// The `ZvalRef` names a bucket of that table, so it is a name only
    /// until the table is written again ([`php_heap::Arr::value_ref`]);
    /// the one opline between this and the write through it is the
    /// `ASSIGN*` php pairs it with, which inserts nothing.
    pub(crate) fn dynamic_property_ref(
        &mut self,
        object: PhpObject,
        name: PhpString,
    ) -> Result<Option<php_heap::ZvalRef>, Thrown> {
        let o = object.heap();
        let key = php_heap::Key::Str(name.heap());
        let held = o.properties().is_some_and(|table| table.contains(key));
        if !held
            && let Err(thrown) = self.dynamic_property_creation(object.class(), name.as_bytes())
        {
            self.release(Value::string(name));
            return Err(thrown);
        }
        let table = self.heap.object_properties(o);
        let table = self.fatal_of(table)?;
        let table = self.array_separate(table)?;
        self.heap.object_set_properties(o, table);
        if !held {
            self.array_update(table, key, Value::null())?;
        }
        self.release(Value::string(name));
        Ok(table.value_ref(key))
    }

    /// The properties in order, the declared slots that hold a value
    /// then the dynamic table, which is the order
    /// `rebuild_object_properties` lays the table out in. Views, and a
    /// declared name is the interned string php's `prop_info->name` is,
    /// never refcounted, so a caller that takes an owner of it takes
    /// none; the `Err` is the memory limit refusing the intern.
    pub(crate) fn object_entries(&mut self, o: Obj) -> Result<Vec<(PhpString, Value)>, Thrown> {
        let mut entries = Vec::new();
        let declared: Vec<Box<[u8]>> = self
            .class_decl(ClassId(o.class()))
            .map(|decl| {
                decl.properties
                    .iter()
                    .filter(|p| p.is_instance)
                    .map(|p| p.name.0.clone())
                    .collect()
            })
            .unwrap_or_default();
        for (i, name) in declared.iter().enumerate() {
            let z = o.slot(u32::try_from(i).unwrap_or(u32::MAX));
            if z.is_undef() {
                continue;
            }
            let interned = self.heap.intern(name);
            let name = self.fatal_of(interned).map(PhpString::of)?;
            entries.push((name, Value::of_zval(z)));
        }
        if let Some(table) = o.properties() {
            for (k, z) in &table {
                if let php_heap::Key::Str(s) = k {
                    entries.push((PhpString::of(s), Value::of_zval(z)));
                }
            }
        }
        Ok(entries)
    }

    /// `zend_objects_clone_obj`, the `Std` `clone_obj` slot: a fresh
    /// entry of the same class holding what this one holds, one more
    /// owner each, and the class's `__clone` served on it, the copy held
    /// at one more owner meanwhile and given up with what `__clone`
    /// threw. An object with a native payload is its class's own
    /// `clone_obj` to copy, since the heap cannot copy a payload it only
    /// knows how to drop; `Std` reaching one is the internal-error fatal
    /// naming the class.
    pub(crate) fn clone_members(&mut self, object: PhpObject) -> Result<PhpObject, Thrown> {
        let cloned = self.heap.object_clone(object.heap());
        let Some(new) = self.fatal_of(cloned)? else {
            let class = self.class_name(object.class());
            return self.fatal_of(Err(Fatal::Error(format!(
                "InternalError: Std::clone_obj over an object of class {class}, whose native payload its own table must copy"
            ))));
        };
        let new = PhpObject::of(new);
        if self.method_named(object.class(), b"__clone").is_some()
            && let Err(thrown) = crate::api::call::call_method(self, &new, b"__clone", &[])
        {
            self.release(new.as_value());
            return Err(thrown);
        }
        Ok(new)
    }

    /// `zend_objects_clone_obj_with`, the `Std` `clone_obj_with` slot:
    /// the class's `clone_obj`, then each of `properties` written on the
    /// copy through the class's `write_property`, a reference with one
    /// owner followed and one with more php's `Error`; the copy is given
    /// up with what a write threw. The scope php writes under as
    /// `fake_scope` is not modelled, so the writes are the executed
    /// scope's.
    pub(crate) fn clone_members_with(
        &mut self,
        object: PhpObject,
        scope: Option<ClassId>,
        properties: PhpArray,
    ) -> Result<PhpObject, Thrown> {
        let _ = scope;
        let table = self.object_handlers(object.heap());
        let Some(clone_obj) = table.clone_obj() else {
            return Err(self.uncloneable(object));
        };
        let new = clone_obj(self, object)?;
        for (key, value) in properties.snapshot() {
            let value = match value.as_php_ref() {
                Some(r) if r.as_value().refcount() > 1 => {
                    self.release(new.as_value());
                    return Err(crate::api::raising::throw(
                        self,
                        "Error",
                        "Cannot assign by reference when cloning with updated properties",
                    ));
                }
                Some(_) => value.deref(),
                None => value,
            };
            let name = match key.to_prop_name(self) {
                Ok(name) => name,
                Err(thrown) => {
                    self.release(new.as_value());
                    return Err(thrown);
                }
            };
            self.addref(&value);
            if let Err(thrown) = table.write_property(self, new, name, value) {
                self.release(new.as_value());
                return Err(thrown);
            }
        }
        Ok(new)
    }

    /// php's `Error` for a class whose `clone_obj` slot is null.
    fn uncloneable(&mut self, object: PhpObject) -> Thrown {
        let class = self.class_name(object.class());
        crate::api::raising::throw(
            self,
            "Error",
            &format!("Trying to clone an uncloneable object of class {class}"),
        )
    }

    /// The body `ZEND_CLONE` and the `clone()` function share
    /// (`Z/Zend/zend_vm_def.h:6010`, `Z/Zend/zend_builtin_functions.c:72`):
    /// a null `clone_obj` is php's `Error` for an uncloneable object,
    /// read before anything else; a `__clone` that is not public is held
    /// to `zend_check_method_accessible` from the executed scope, the
    /// `Error` `zend_bad_method_call`'s; then the class's `clone_obj`,
    /// or its `clone_obj_with` where `with` names properties, php's
    /// `Error` where the class has none. The copy is the caller's, one
    /// owner.
    pub(crate) fn clone_object(
        &mut self,
        object: PhpObject,
        with: Option<PhpArray>,
    ) -> Result<PhpObject, Thrown> {
        let table = self.object_handlers(object.heap());
        let Some(clone_obj) = table.clone_obj() else {
            return Err(self.uncloneable(object));
        };
        if let Some(clone) = self.method_named(object.class(), b"__clone") {
            self.check_method_access(clone, b"__clone")?;
        }
        match with.filter(|with| !with.is_empty()) {
            Some(with) => {
                let Some(clone_obj_with) = table.clone_obj_with() else {
                    let class = self.class_name(object.class());
                    return Err(crate::api::raising::throw(
                        self,
                        "Error",
                        &format!(
                            "Cloning objects of class {class} with updated properties is not supported"
                        ),
                    ));
                };
                let scope = self.executed_scope();
                clone_obj_with(self, object, scope, with)
            }
            None => clone_obj(self, object),
        }
    }

    /// A class's `clone_obj` over an object with a native payload: the
    /// members copied as `clone_members` copies them and `payload`, the
    /// class's copy of the old one, in the new object's slot. What the
    /// copy names of the heap is the caller's to have taken an owner of.
    pub(crate) fn clone_with_payload<T: std::any::Any + Send>(
        &mut self,
        object: PhpObject,
        payload: T,
    ) -> Result<PhpObject, Thrown> {
        let cloned = self.heap.object_clone_with(object.heap(), payload);
        self.fatal_of(cloned).map(PhpObject::of)
    }

    /// The native payload of `o` as `T`, borrowed for as long as the
    /// request is.
    pub(crate) fn native<T: std::any::Any + Send>(&self, o: Obj) -> Option<&T> {
        self.heap.native(o)
    }

    /// The native payload of `o` as `T`, mutably.
    pub(crate) fn native_mut<T: std::any::Any + Send>(&mut self, o: Obj) -> Option<&mut T> {
        self.heap.native_mut(o)
    }

    /// `zend_objects_destroy_object` (`Z/Zend/zend_objects.c:114`): the
    /// class's `__destruct` served on the object, which the store holds
    /// at one owner meanwhile (`zend_objects_store_del` holds the count
    /// there, where php's `GC_ADDREF` and `OBJ_RELEASE` around the call
    /// hold it). A class with none serves nothing, and so does a lazy
    /// object, which is the first test php makes after finding the
    /// destructor (`zend_object_is_lazy`); this engine makes none,
    /// `newLazyGhost` and `newLazyProxy` being what does, so that branch
    /// is php's sentence and no code here. The visibility check and the
    /// pending-throwable rules are the branches under this one.
    ///
    /// An array gives its elements up in element order
    /// (`zend_array_destroy` over `i_zval_ptr_dtor`), so the
    /// destructors of the objects it held are served in that order, and
    /// the array is gone before the statement after it runs.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; }
    /// }
    /// $a = [new A(1), new A(2)];
    /// $a = 0;
    /// echo "after\n";
    /// ```
    /// ```expect
    /// d1
    /// d2
    /// after
    /// ```
    ///
    /// A destructor that takes an owner of its object resurrects it:
    /// the release that served it finds a count above zero and frees
    /// nothing, and the object is never served again, because
    /// `IS_OBJ_DESTRUCTOR_CALLED` stands on it for the rest of the
    /// request.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     public $self;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; $this->self = $this; }
    /// }
    /// $a = new A(1);
    /// $a = 0;
    /// echo "after\n";
    /// ```
    /// ```expect
    /// d1
    /// after
    /// ```
    pub(crate) fn destruct(&mut self, object: PhpObject) {
        if self.marked_destructed {
            return;
        }
        let class = object.class();
        let Some(destructor) = self.method_named(class, b"__destruct") else {
            return;
        };
        if !self.destructor_reachable(object, destructor) {
            return;
        }
        let saved = self.save_pending(object);
        if matches!(saved, Saved::TheObject) {
            return;
        }
        if let Err(thrown) = self.serve_method(destructor, Some(object), class, &[]) {
            self.leave_pending(&thrown);
            if !self.executing() {
                self.end_the_walk();
                return;
            }
        }
        self.restore_pending(saved);
    }

    /// `zend_call_function`'s tail (`Z/Zend/zend_execute_API.c:1069-1076`)
    /// where a destructor left a throwable pending and nothing is
    /// executing: php calls `zend_throw_exception_internal(NULL)`, which
    /// renders the uncaught fatal there and `zend_bailout`s
    /// (`Z/Zend/zend_exceptions.c:203-222`); `shutdown_destructors`
    /// catches that `zend_bailout` and calls
    /// `zend_objects_store_mark_destructed`, so neither walk serves
    /// another destructor. The fatal is rendered here rather than left
    /// for the end of the walk, which is what keeps it one throwable
    /// with no `Next` (design §5.10).
    ///
    /// The rule is `zend_call_function`'s and not the walk's: a raise
    /// inside a serving has a frame to land in and is the pending
    /// throwable of that frame, which is the branch above this one.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     public $self;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; nope(); }
    /// }
    /// $a = new A(1);
    /// $a->self = $a;
    /// $b = new A(2);
    /// $b->self = $b;
    /// echo "end\n";
    /// ```
    /// ```expect exit=255
    /// end
    /// d1
    ///
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:6
    /// Stack trace:
    /// #0 [internal function]: A->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 6
    /// ```
    ///
    /// The symbol table's walk ends the same way, and it runs first, so
    /// a raise there leaves the store's walk nothing to serve: `$b` is
    /// deleted before `$a`, as `zend_hash_reverse_apply` takes them, and
    /// `$a`'s destructor is never served.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; nope(); }
    /// }
    /// $a = new A(1);
    /// $b = new A(2);
    /// echo "end\n";
    /// ```
    /// ```expect exit=255
    /// end
    /// d2
    ///
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:5
    /// Stack trace:
    /// #0 [internal function]: A->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 5
    /// ```
    fn end_the_walk(&mut self) {
        self.marked_destructed = true;
        if let Err(fatal) = self.uncaught() {
            self.end_in(&fatal);
        }
    }

    /// The first half of `zend_objects_destroy_object`'s protection of
    /// a destructor from a throwable raised before it
    /// (`Z/Zend/zend_objects.c:148-168`): the pending slot is emptied
    /// for the call, and what it held is handed back for
    /// [`Request::restore_pending`]. A destructor served on the pending
    /// throwable itself is php's one `E_CORE_ERROR`, and the `None`
    /// here is that: the request is ending and nothing is served.
    ///
    /// No PHP snippet reaches that fatal, in this engine or in php,
    /// whose own corpus has no test naming it (harness §8). It would
    /// take the pending throwable's last owner released while the slot
    /// still holds it, and php releases one in that state in two places
    /// only: `zend_clear_exception` empties the slot before
    /// `OBJ_RELEASE`, under php's own comment that the throwable may
    /// have a destructor (`Z/Zend/zend_exceptions.c:251-254`), and
    /// `zend_user_exception_handler` releases `EG(exception)` before
    /// emptying it (`Z/Zend/zend.c:1949-1953`) but is reached only with
    /// `EG(current_execute_data)` null, where `zend_call_function`'s
    /// tail renders the handler's own throwable uncaught and
    /// `zend_bailout`s first. `zend_exception_set_previous`, called from
    /// `zend_throw_exception_internal` before the slot is reassigned
    /// (`Z/Zend/zend_exceptions.c:201-202`), gives up the slot's object
    /// only where a `previous` chain or the throw itself holds it a
    /// second time. This engine's slot is the one owner of what stands
    /// in it and nothing releases that while it stands, so the state is
    /// unreachable here for its own reason as well. The unit test beside
    /// this builds what the release cannot, and #458 is the question
    /// that leaves for a person.
    fn save_pending(&mut self, object: PhpObject) -> Saved {
        match self.pending {
            Some(pending) if pending.index() == object.index() => {
                self.end_in_core_error("Attempt to destruct pending exception");
                Saved::TheObject
            }
            _ => match self.pending.take() {
                Some(pending) => Saved::Throwable(pending),
                None => Saved::Nothing,
            },
        }
    }

    /// The second half: the throwable the destructor was protected from
    /// is pending again, and one the destructor itself raised keeps the
    /// slot with the old one chained onto it as its `previous`
    /// (`zend_exception_set_previous`), which is what the rendering
    /// prints under `Next`.
    ///
    /// The array here gives its two elements up in order, and the first
    /// destructor's raise is pending while the second is served: it is
    /// saved, the second destructor writes its own line and raises in
    /// its turn, and the two are one uncaught throwable with the first
    /// under `Next`.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; nope(); }
    /// }
    /// $a = [new A(1), new A(2)];
    /// $a = 0;
    /// echo "after\n";
    /// ```
    /// ```expect exit=255
    /// d1
    /// d2
    ///
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:5
    /// Stack trace:
    /// #0 %s(8): A->__destruct()
    /// #1 {main}
    ///
    /// Next Error: Call to undefined function nope() in %s:5
    /// Stack trace:
    /// #0 %s(8): A->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 5
    /// ```
    fn restore_pending(&mut self, saved: Saved) {
        let Saved::Throwable(old) = saved else {
            return;
        };
        match self.pending {
            Some(new) => self.set_previous(new, old),
            None => self.pending = Some(old),
        }
    }

    /// `zend_exception_set_previous` (`Z/Zend/zend_exceptions.c:103`):
    /// `previous` is chained onto the end of the chain `throwable`
    /// already carries, its one owner the property's from here. A
    /// throwable that is already in that chain, or is `throwable`
    /// itself, takes its owner back instead, since php refuses to make
    /// a cycle. A chain is a path through live objects, so the store's
    /// count is a length no walk of it passes.
    pub(crate) fn set_previous(&mut self, throwable: PhpObject, previous: PhpObject) {
        let mut at = throwable;
        for _ in 0..=self.heap.objects() {
            if at.index() == previous.index() {
                break;
            }
            if let Some(next) = self.previous_of(at) {
                at = next;
                continue;
            }
            let Ok(name) = PhpString::new(self, b"previous") else {
                return;
            };
            if at.set_prop(self, name, previous.as_value()).is_ok() {
                return;
            }
            break;
        }
        self.release(previous.as_value());
    }

    /// The `previous` property of a throwable, when it holds one.
    fn previous_of(&self, throwable: PhpObject) -> Option<PhpObject> {
        throwable
            .prop(self, b"previous")
            .and_then(|value| value.as_object().copied())
    }

    /// `zend_objects_destroy_object`'s visibility branch: a public
    /// `__destruct` is served from anywhere, and a private or a
    /// protected one is held to `zend_check_method_accessible` from the
    /// executed scope while a frame stands, php's `Error` naming the
    /// visibility, the object's class and the scope the release came
    /// from. With no frame standing, which is the store's shutdown walk
    /// ([`Request::call_destructors`]) and php's `EG(current_execute_data)`
    /// being null, there is no scope to check and the destructor is
    /// skipped with a warning instead.
    ///
    /// ```php
    /// class A { private function __destruct() { echo "never\n"; } }
    /// $a = new A();
    /// $a = 1;
    /// echo "after\n";
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Call to private A::__destruct() from global scope in %s:4
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 4
    /// ```
    ///
    /// ```php
    /// class A { public $self; private function __destruct() { echo "never\n"; } }
    /// $a = new A();
    /// $a->self = $a;
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    ///
    /// Warning: Call to private A::__destruct() from global scope during shutdown ignored in Unknown on line 0
    /// ```
    fn destructor_reachable(&mut self, object: PhpObject, destructor: Function) -> bool {
        let visibility = self.method_visibility(destructor);
        if visibility == Visibility::Public {
            return true;
        }
        let shown = visibility_string(visibility);
        let class = self.class_name(object.class());
        if !self.executing() {
            crate::api::raising::warning(
                self,
                &format!(
                    "Call to {shown} {class}::__destruct() from global scope during shutdown ignored"
                ),
            );
            return false;
        }
        if self.method_accessible(destructor) {
            return true;
        }
        let from = self.scope_phrase();
        let thrown = crate::api::raising::throw(
            self,
            "Error",
            &format!("Call to {shown} {class}::__destruct() from {from}"),
        );
        self.leave_pending(&thrown);
        false
    }

    /// `EG(current_execute_data)`: whether a frame or a native's call
    /// stands, which is every release inside a serving and none of the
    /// releases `php_request_shutdown` makes after the main frame popped.
    fn executing(&self) -> bool {
        self.vm.innermost().is_some() || self.vm.serving_call().is_some()
    }

    /// `zend_detach_symbol_table` (`Z/Zend/zend_execute_API.c:1892`),
    /// what php does when it leaves the main op array's frame, the one
    /// it marks `ZEND_CALL_TOP_CODE` and gives `EG(symbol_table)` as
    /// its own (design §5.6): every CV that holds something is moved
    /// into the symbol table under the name the compiler numbered it
    /// by, the slot left `UNDEF` so the pop releases nothing, and a
    /// name whose CV holds nothing is deleted from the table. The
    /// variables are the request's from here and outlive the frame,
    /// which is why `$a = new A;` is still an owner while the shutdown
    /// functions are served and the destructor waits for
    /// [`Request::shutdown_destructors`].
    pub(crate) fn detach_symbol_table(&mut self, frame: FrameRef, unit: &Unit) {
        let names: Vec<Vec<u8>> = exec::code_of(unit)
            .vars
            .iter()
            .map(|name| name.0.to_vec())
            .collect();
        for (at, name) in names.into_iter().enumerate() {
            let value = self.take_slot(frame, u32::try_from(at).unwrap_or(u32::MAX));
            if value.tag() == TypeTag::Undef {
                self.symbol_del(&name);
            } else {
                self.symbol_update(name, value);
            }
        }
    }

    /// One symbol written, `zend_hash_update`: `value` is the table's
    /// and what stood under the name is released after the write, the
    /// name keeping the position it had, as php's update keeps a
    /// bucket's and only a name the table has not appends.
    fn symbol_update(&mut self, name: Vec<u8>, value: Value) {
        if let Some(at) = self.symbols.iter().position(|(key, _)| *key == name) {
            let old = std::mem::replace(&mut self.symbols[at].1, value);
            self.release(old);
            return;
        }
        self.symbols.push((name, value));
    }

    /// One symbol deleted, `zend_hash_del`: the name gone and what it
    /// held released; a name the table has not is no error, as php's
    /// delete of a missing key is none.
    fn symbol_del(&mut self, name: &[u8]) {
        let Some(at) = self.symbols.iter().position(|(key, _)| key == name) else {
            return;
        };
        let (_, value) = self.symbols.remove(at);
        self.release(value);
    }

    /// `shutdown_destructors` (`Z/Zend/zend_execute_API.c:251`), the
    /// first half of `zend_call_destructors` and so the first
    /// destructor php serves at shutdown (design §5.10): the symbol
    /// table walked in reverse, `zend_hash_reverse_apply` over
    /// `zval_call_destructor`, each symbol that holds an object nothing
    /// else owns deleted, which serves that object's destructor; the
    /// walk repeated while the count of symbols moves, since one
    /// destructor's work can drop another symbol's last other owner.
    /// [`Request::call_destructors`], the store's walk, runs after it
    /// and takes what is left.
    ///
    /// So two top-level variables holding an object each are served in
    /// reverse declaration order, the order the CVs were numbered read
    /// backwards, and not in the order the store made them.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; }
    /// }
    /// $a = new A(1);
    /// $b = new A(2);
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    /// d2
    /// d1
    /// ```
    ///
    /// A global whose object has an owner beside the symbol is kept by
    /// the walk and served by the store's after it, in store order, so
    /// the two orders are visible in one program: `$b`, owned by its
    /// symbol alone, is served first and backwards; `$a` and `$c`, each
    /// holding itself, are served after it and forwards.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     public $self;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; }
    /// }
    /// $a = new A(1);
    /// $a->self = $a;
    /// $b = new A(2);
    /// $c = new A(3);
    /// $c->self = $c;
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    /// d2
    /// d1
    /// d3
    /// ```
    pub(crate) fn shutdown_destructors(&mut self) {
        loop {
            let symbols = self.symbols.len();
            let mut at = self.symbols.len();
            while at > 0 && !self.marked_destructed {
                at -= 1;
                let alone = self
                    .symbols
                    .get(at)
                    .is_some_and(|(_, value)| value.as_object().is_some() && value.refcount() == 1);
                if alone {
                    let (_, value) = self.symbols.remove(at);
                    self.release(value);
                }
            }
            if symbols == self.symbols.len() || self.marked_destructed {
                break;
            }
        }
    }

    /// `zend_hash_graceful_reverse_destroy(&EG(symbol_table))` in
    /// `zend_shutdown_executor_values`
    /// (`Z/Zend/zend_execute_API.c:271`), after the open resources are
    /// closed and before the object storage is freed: every symbol left
    /// released from the last to the first. The destructors are behind
    /// the request by now, `zend_objects_store_call_destructors` having
    /// marked every object, so this frees and serves nothing.
    pub(crate) fn destroy_symbol_table(&mut self) {
        while let Some((_, value)) = self.symbols.pop() {
            self.release(value);
        }
    }

    /// `zend_objects_store_call_destructors`, the second half of
    /// `zend_call_destructors` (design §5.10): every live object's
    /// destructor not yet served, in store order, until one leaves a
    /// throwable pending, which [`Request::end_the_walk`] renders where
    /// php renders it and ends both walks with. What the symbol table
    /// owned alone was served before this by
    /// [`Request::shutdown_destructors`]. The store's walk still marks
    /// every object it passes, as php's `zend_objects_store_mark_destructed`
    /// marks them behind the `zend_bailout`, so nothing is served after
    /// it either.
    ///
    /// An object a variable no longer names alone outlives that walk,
    /// so the store's is what serves it: each of these holds itself,
    /// so their destructors are served after the last statement, and in
    /// the order the store made them, which is not the order the
    /// variables were written.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     public $self;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; }
    /// }
    /// $a = new A(1);
    /// $a->self = $a;
    /// $b = new A(2);
    /// $b->self = $b;
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    /// d1
    /// d2
    /// ```
    pub(crate) fn call_destructors(&mut self) {
        Heap::call_destructors(&mut Hooks(self));
        if let Err(fatal) = self.uncaught() {
            self.end_in(&fatal);
        }
    }

    /// `zend_objects_store_free_object_storage`, in `shutdown_executor`
    /// after the extensions' `rshutdown`: every live object's members
    /// and payload freed once, the bytes left for the mass release.
    pub(crate) fn free_object_storage(&mut self) {
        Heap::free_object_storage(&mut Hooks(self));
    }

    /// The handler table an object of `class` is reached through
    /// (design §5.4): the class's own where it declared one, else the
    /// nearest ancestor's, as php inherits `create_object` and the table
    /// it installs, and `Std`'s at a root and for every user class. The
    /// walk is cut at a depth no declaration can make.
    pub(crate) fn handlers_of(&self, class: ClassId) -> &'static dyn ObjectHandlers {
        let mut at = Some(class);
        for _ in 0..64 {
            let Some(class) = at else {
                break;
            };
            if let Some(Some(table)) = usize::try_from(class.0)
                .ok()
                .and_then(|i| self.shared.handlers.get(i))
            {
                return *table;
            }
            at = self.class_parent(class);
        }
        &Std
    }

    /// The `create_object` of the class or of the nearest parent that
    /// declares one (design §5.4), php's inherited `ce->create_object`;
    /// `None` for a class whose objects carry no native payload, which
    /// is `zend_objects_new`.
    pub(crate) fn native_new_of(&self, class: ClassId) -> Option<NativeNew> {
        let mut at = Some(class);
        for _ in 0..64 {
            let class = at?;
            if let Some(Some(new)) = usize::try_from(class.0)
                .ok()
                .and_then(|i| self.shared.natives.get(i))
            {
                return Some(*new);
            }
            at = self.class_parent(class);
        }
        None
    }

    /// [`Request::handlers_of`] for the object's class, php's
    /// `zobj->handlers`.
    pub(crate) fn object_handlers(&self, o: Obj) -> &'static dyn ObjectHandlers {
        self.handlers_of(ClassId(o.class()))
    }

    /// `zend_std_get_property_ptr_ptr` without the write: the declared
    /// slot `name` sits in where the executed scope may reach it, and
    /// none for a dynamic name or a property the scope may not see,
    /// which the read and the write handlers take instead.
    pub(crate) fn property_slot(&self, object: PhpObject, name: &[u8]) -> Option<u32> {
        let class = object.class();
        let placed = self.declared_slot(class, name)?;
        self.may_access(class, placed.visibility)
            .then_some(placed.slot)
    }

    /// `zend_std_has_property`: the declared slot where the scope may
    /// reach it and it holds a value, else the dynamic table's entry;
    /// then what `test` asks of the value: that it is there, that it is
    /// not `null`, or that it is truthy. A slot the scope may not reach
    /// or nothing wrote is `false`, as php answers with no `__isset`.
    pub(crate) fn has_property(
        &mut self,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> bool {
        let class = object.class();
        let value = match self.declared_slot(class, name) {
            Some(placed) => {
                if !self.may_access(class, placed.visibility) {
                    return false;
                }
                let z = object.heap().slot(placed.slot);
                (!z.is_undef()).then(|| Value::of_zval(z))
            }
            None => self.object_prop(object.heap(), name),
        };
        let Some(value) = value else {
            return false;
        };
        let value = value.deref();
        match test {
            PropertyTest::Exists => true,
            PropertyTest::Isset => value.tag() != TypeTag::Null,
            PropertyTest::NotEmpty => crate::api::coerce::truthy(self, &value),
        }
    }

    /// `zend_std_unset_property`: a declared slot the scope may reach is
    /// left uninitialised, what it held released; one it may not reach
    /// is php's `Error`, and so is one whose set visibility the scope
    /// may not write, whose word here is `unset`
    /// (`Z/Zend/zend_object_handlers.c:1583`); a dynamic name is
    /// removed from the table, separated first where a cast shares it.
    pub(crate) fn unset_property(&mut self, object: PhpObject, name: &[u8]) -> Result<(), Thrown> {
        let class = object.class();
        let o = object.heap();
        if let Some(placed) = self.declared_slot(class, name) {
            if !self.may_access(class, placed.visibility) {
                return Err(self.inaccessible_property(class, placed.visibility, name));
            }
            if let Some(set) = placed.set_visibility
                && !self.has_set_access(class, set)
            {
                return Err(self.set_inaccessible_property(class, set, name, "unset"));
            }
            Heap::object_set_slot(&mut Hooks(self), o, placed.slot, Zval::UNDEF);
            return Ok(());
        }
        let Some(table) = o.properties() else {
            return Ok(());
        };
        let table = self.array_separate(table)?;
        self.heap.object_set_properties(o, table);
        let key = self.interned(name)?;
        self.array_del(table, php_heap::Key::Str(key.heap()));
        Ok(())
    }

    /// `zend_class_implements_interface(ce, zend_ce_arrayaccess)`: the
    /// object's class where it is an `ArrayAccess`, else
    /// `zend_bad_array_access`, php's `Error` for an object used as an
    /// array.
    fn array_access(&mut self, object: PhpObject) -> Result<(), Thrown> {
        let implements = self
            .class_named(b"ArrayAccess")
            .is_some_and(|interface| self.instance_of(object.class(), interface));
        if implements {
            return Ok(());
        }
        let class = self.class_name(object.class());
        Err(crate::api::raising::throw(
            self,
            "Error",
            &format!("Cannot use object of type {class} as array"),
        ))
    }

    /// `zend_std_read_dimension`: `offsetGet` on an `ArrayAccess`, the
    /// offset `null` for `$o[]`.
    pub(crate) fn read_dimension(
        &mut self,
        object: PhpObject,
        offset: Option<&Value>,
    ) -> Result<Value, Thrown> {
        self.array_access(object)?;
        let offset = offset.copied().unwrap_or_else(Value::null);
        crate::api::call::call_method(self, &object, b"offsetGet", &[offset])
    }

    /// `zend_std_read_dimension` under `BP_VAR_IS`: `offsetExists`
    /// first, `null` where it says no, and `offsetGet` after it where it
    /// says yes, so a chain under an `isset` calls the two in that order
    /// and calls neither on a class that is no `ArrayAccess`.
    pub(crate) fn read_dimension_is(
        &mut self,
        object: PhpObject,
        offset: Option<&Value>,
    ) -> Result<Value, Thrown> {
        self.array_access(object)?;
        let offset = offset.copied().unwrap_or_else(Value::null);
        let exists = crate::api::call::call_method(
            self,
            &object,
            b"offsetExists",
            std::slice::from_ref(&offset),
        )?;
        let answer = crate::api::coerce::truthy(self, &exists);
        self.release(exists);
        if !answer {
            return Ok(Value::null());
        }
        crate::api::call::call_method(self, &object, b"offsetGet", &[offset])
    }

    /// `zend_std_write_dimension`: `offsetSet` on an `ArrayAccess`, the
    /// offset `null` for `$o[] = v`; `value` is given up either way.
    pub(crate) fn write_dimension(
        &mut self,
        object: PhpObject,
        offset: Option<&Value>,
        value: Value,
    ) -> Result<(), Thrown> {
        if let Err(thrown) = self.array_access(object) {
            self.release(value);
            return Err(thrown);
        }
        let offset = offset.copied().unwrap_or_else(Value::null);
        let served = crate::api::call::call_method(self, &object, b"offsetSet", &[offset, value]);
        self.release(value);
        self.release(served?);
        Ok(())
    }

    /// `zend_std_has_dimension`: `offsetExists` on an `ArrayAccess`,
    /// and under `not_empty` the truth of `offsetGet` after it.
    pub(crate) fn has_dimension(
        &mut self,
        object: PhpObject,
        offset: &Value,
        not_empty: bool,
    ) -> Result<bool, Thrown> {
        self.array_access(object)?;
        let exists = crate::api::call::call_method(
            self,
            &object,
            b"offsetExists",
            std::slice::from_ref(offset),
        )?;
        let mut answer = crate::api::coerce::truthy(self, &exists);
        self.release(exists);
        if answer && not_empty {
            let value = crate::api::call::call_method(
                self,
                &object,
                b"offsetGet",
                std::slice::from_ref(offset),
            )?;
            answer = crate::api::coerce::truthy(self, &value);
            self.release(value);
        }
        Ok(answer)
    }

    /// `zend_std_unset_dimension`: `offsetUnset` on an `ArrayAccess`.
    pub(crate) fn unset_dimension(
        &mut self,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        self.array_access(object)?;
        let served = crate::api::call::call_method(
            self,
            &object,
            b"offsetUnset",
            std::slice::from_ref(offset),
        )?;
        self.release(served);
        Ok(())
    }

    /// `zend_std_get_constructor`: the class's `__construct` where the
    /// executed scope may reach it, `zend_bad_constructor_call`'s
    /// `Error` where it may not, and none for a class with no
    /// constructor.
    pub(crate) fn constructor_of(&mut self, object: PhpObject) -> Result<Option<Function>, Thrown> {
        let Some(constructor) = self.method_named(object.class(), b"__construct") else {
            return Ok(None);
        };
        self.check_method_access(constructor, b"__construct")?;
        Ok(Some(constructor))
    }

    /// `zend_std_cast_object_tostring`: `__toString` for a string, its
    /// value where it is one and php's `Error` where it is not; `true`
    /// for a bool; and none for every other type and for a class with no
    /// `__toString`.
    pub(crate) fn cast_object(
        &mut self,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Value>, Thrown> {
        match to {
            TypeTag::String => {
                if self.method_named(object.class(), b"__tostring").is_none() {
                    return Ok(None);
                }
                let value = crate::api::call::call_method(self, &object, b"__toString", &[])?;
                if value.as_php_string().is_some() {
                    return Ok(Some(value));
                }
                self.release(value);
                let class = self.class_name(object.class());
                Err(crate::api::raising::throw(
                    self,
                    "Error",
                    &format!("Method {class}::__toString() must return a string value"),
                ))
            }
            TypeTag::Bool => Ok(Some(Value::bool(true))),
            _ => Ok(None),
        }
    }

    /// `zend_std_get_debug_info`: what `__debugInfo` returns, an array's
    /// rows or none for `null`, and `E_ERROR` for anything else; the
    /// properties for a class with no `__debugInfo`.
    pub(crate) fn debug_info(
        &mut self,
        object: PhpObject,
    ) -> Result<Vec<(PhpString, Value)>, Thrown> {
        if self.method_named(object.class(), b"__debuginfo").is_none() {
            return self.object_entries(object.heap());
        }
        let value = crate::api::call::call_method(self, &object, b"__debugInfo", &[])?;
        let rows = match value.as_array() {
            Some(array) => {
                let mut rows = Vec::new();
                for (key, element) in array.snapshot() {
                    rows.push((key.to_prop_name(self)?, element));
                }
                rows
            }
            None if value.tag() == TypeTag::Null => Vec::new(),
            None => {
                self.release(value);
                return Err(crate::api::raising::fatal(
                    self,
                    "__debuginfo() must return an array",
                ));
            }
        };
        self.release(value);
        Ok(rows)
    }

    /// `zend_std_get_closure`: the class's `__invoke` with the object as
    /// its receiver and the class as its scope, and none for a class
    /// with no `__invoke`.
    pub(crate) fn invoke_closure(&mut self, object: PhpObject) -> Option<ClosureInfo> {
        let function = self.method_named(object.class(), b"__invoke")?;
        Some(ClosureInfo {
            function,
            this: Some(object),
            scope: Some(object.class()),
            bound: Vec::new(),
            fake: true,
        })
    }

    /// `zend_std_compare_objects` (`Z/Zend/zend_object_handlers.c:2227`),
    /// the `Std` `compare` slot. An object against a non-object is the
    /// cast path: `cast_object` to the other side's type, a refusal for
    /// an int or a float php's notice and `1` or `1.0` in the object's
    /// place, a refusal for anything else the object as the greater side,
    /// and the cast compared by [`crate::ops::compare`] in the object's
    /// position. Two objects are equal when one; uncomparable, `None`,
    /// across classes; and within one class, while neither carries a
    /// dynamic table, each declared slot in turn, both uninitialised
    /// passing, one uninitialised `1`, both held compared as `zend_compare`
    /// compares them, the left object protected for the walk and the
    /// `Error` php throws when the walk reaches it again. Where either
    /// carries a dynamic table it is `zend_compare_symbol_tables` over
    /// the two rebuilt tables: the counts first, then the left one's keys
    /// looked up in the right one, an uninitialised slot `-1` against a
    /// held one and `1` the other way.
    ///
    /// php opens the handler with `zend_objects_check_stack_limit`
    /// (`Z/Zend/zend_object_handlers.c:2231`) and throws
    /// `Maximum call stack size reached during object comparison` where
    /// the thread has come down to `zend.max_allowed_stack_size`, which
    /// is [`php_compile::StackLimit::overflowed`] here and is asked in
    /// the same place, before either side is even read as an object. It
    /// ends a walk no recursion guard can: a chain of objects deep
    /// enough to exhaust the C stack without ever reaching one twice.
    pub(crate) fn compare_objects(
        &mut self,
        left: &Value,
        right: &Value,
    ) -> Result<Option<std::cmp::Ordering>, Thrown> {
        use std::cmp::Ordering;
        if self.stack_limit.overflowed() {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                "Maximum call stack size reached during object comparison",
            ));
        }
        let (a, b) = match (left.as_object().copied(), right.as_object().copied()) {
            (Some(a), Some(b)) => (a, b),
            (Some(object), None) => return self.compare_cast(object, right, true).map(Some),
            (None, Some(object)) => return self.compare_cast(object, left, false).map(Some),
            (None, None) => return Ok(None),
        };
        if a == b {
            return Ok(Some(Ordering::Equal));
        }
        if a.class() != b.class() {
            return Ok(None);
        }
        let (x, y) = (a.heap(), b.heap());
        // `GC_ADDREF` on both for the walk, given up at `done`.
        self.addref(left);
        self.addref(right);
        let order =
            if x.properties().is_none() && y.properties().is_none() && !x.is_lazy() && !y.is_lazy()
            {
                self.compare_slots(x, y)
            } else {
                self.compare_tables(x, y)
            };
        self.release(*left);
        self.release(*right);
        order.map(Some)
    }

    /// The declared-slot walk of [`Request::compare_objects`], under the
    /// recursion guard on the left object; it is enough to protect one,
    /// as php notes, since the other may be reached from it.
    fn compare_slots(&mut self, x: Obj, y: Obj) -> Result<std::cmp::Ordering, Thrown> {
        use std::cmp::Ordering;
        if x.slots() == 0 {
            return Ok(Ordering::Equal);
        }
        if x.is_recursive() {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                "Nesting level too deep - recursive dependency?",
            ));
        }
        x.protect();
        let mut order = Ordering::Equal;
        for i in 0..x.slots() {
            let (p, q) = (x.slot(i), y.slot(i));
            order = match (p.is_undef(), q.is_undef()) {
                (true, true) => continue,
                (true, false) | (false, true) => Ordering::Greater,
                (false, false) => {
                    match crate::ops::compare(self, &Value::of_zval(p), &Value::of_zval(q)) {
                        Ok(Ordering::Equal) => continue,
                        Ok(order) => order,
                        Err(thrown) => {
                            x.unprotect();
                            return Err(thrown);
                        }
                    }
                }
            };
            break;
        }
        x.unprotect();
        Ok(order)
    }

    /// `zend_compare_symbol_tables` over the two objects' rebuilt tables
    /// (`rebuild_object_properties_internal`): every declared slot, held
    /// or not, in declaration order and then the dynamic table, so the
    /// count is the slots and the dynamic entries, and `zend_hash_compare`
    /// with `ordered` off over them.
    fn compare_tables(&mut self, x: Obj, y: Obj) -> Result<std::cmp::Ordering, Thrown> {
        use std::cmp::Ordering;
        let dynamic = |o: Obj| o.properties().map_or(0, |t| t.len());
        if dynamic(x) != dynamic(y) {
            return Ok(dynamic(x).cmp(&dynamic(y)));
        }
        for i in 0..x.slots() {
            let (p, q) = (x.slot(i), y.slot(i));
            match (p.is_undef(), q.is_undef()) {
                (true, true) => {}
                (true, false) => return Ok(Ordering::Less),
                (false, true) => return Ok(Ordering::Greater),
                (false, false) => {
                    let order = crate::ops::compare(self, &Value::of_zval(p), &Value::of_zval(q))?;
                    if order != Ordering::Equal {
                        return Ok(order);
                    }
                }
            }
        }
        let (Some(p), Some(q)) = (x.properties(), y.properties()) else {
            return Ok(Ordering::Equal);
        };
        crate::ops::compare(
            self,
            &Value::array(PhpArray::of(p)),
            &Value::array(PhpArray::of(q)),
        )
    }

    /// The cast path of [`Request::compare_objects`]: the object's
    /// `cast_object` to the other side's type, `object_lhs` saying which
    /// side the object is on, as `zend_std_compare_objects` reads it.
    fn compare_cast(
        &mut self,
        object: PhpObject,
        other: &Value,
        object_lhs: bool,
    ) -> Result<std::cmp::Ordering, Thrown> {
        use std::cmp::Ordering;
        let target = other.tag();
        let casted = match self
            .object_handlers(object.heap())
            .cast_object(self, object, target)?
        {
            Some(casted) => casted,
            None => match target {
                TypeTag::Int | TypeTag::Float => {
                    let class = self.class_name(object.class());
                    let to = if target == TypeTag::Int {
                        "int"
                    } else {
                        "float"
                    };
                    crate::api::raising::notice(
                        self,
                        &format!("Object of class {class} could not be converted to {to}"),
                    );
                    if target == TypeTag::Int {
                        Value::int(1)
                    } else {
                        Value::float(1.0)
                    }
                }
                _ => {
                    return Ok(if object_lhs {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    });
                }
            },
        };
        let order = if object_lhs {
            crate::ops::compare(self, &casted, other)
        } else {
            crate::ops::compare(self, other, &casted)
        };
        self.release(casted);
        order
    }

    /// `zend_active_function()`: the native being served, which is the
    /// call frame on top of [`Vm`] while an entry runs, and none while
    /// the loop is between calls. The raisers render its declared name
    /// and its parameters' names off the declaration, never off the
    /// caller's spelling.
    fn active_native_function(&self) -> Option<Function> {
        self.vm.serving_call().map(|call| call.function)
    }

    /// `get_active_function_name`: the declared name of the native being
    /// served, `func->common.function_name`, whatever case the call
    /// spelled it in.
    pub(crate) fn active_native(&self) -> String {
        self.active_native_function().map_or_else(
            || String::from("native"),
            |function| self.qualified_name(function, None),
        )
    }

    /// `func->common.function_name`: the name a function was declared
    /// under, and `native` for one the table has no declaration for. A
    /// method's is its own name alone, what `get_active_function_name`
    /// answers; the class before it is [`Request::qualified_name`]'s.
    fn declared_name(&self, function: Function) -> String {
        self.function_decl(function).map_or_else(
            || String::from("native"),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        )
    }

    /// The name a raise or a trace prints a callee under: a function's
    /// declared name, and `Class::method` for a method, as
    /// `zend_wrong_parameters_count_error` and `zend_trace_to_string`
    /// join `func->common.scope->name` to it; a trace joins with `->`
    /// where the call had a receiver.
    fn qualified_name(&self, function: Function, this: Option<PhpObject>) -> String {
        let name = self.declared_name(function);
        match self.method_class(function) {
            Some(class) => {
                let class = self.class_name(class);
                let join = if this.is_some() { "->" } else { "::" };
                format!("{class}{join}{name}")
            }
            None => name,
        }
    }

    /// `get_active_function_arg_name`: the declared name of the active
    /// native's parameter at `position`, one-based, and none past the
    /// declared count, where php's variadic slot sits
    /// (`Z/Zend/zend_execute_API.c:643-654`).
    fn active_native_arg_name(&self, position: u32) -> Option<String> {
        let decl = self.function_decl(self.active_native_function()?)?;
        let parameter = decl
            .parameters
            .get(usize::try_from(position.checked_sub(1)?).ok()?)?;
        (!parameter.variadic).then(|| String::from_utf8_lossy(&parameter.name.0).into_owned())
    }

    /// `arg_info->type` of the active native's parameter at `position`,
    /// what `zend_null_arg_deprecated` names the type by; none where the
    /// native declares no type there.
    pub(crate) fn active_parameter_type(&self, position: u32) -> Option<php_unit::decl::Type> {
        let decl = self.function_decl(self.active_native_function()?)?;
        decl.parameters
            .get(usize::try_from(position.checked_sub(1)?).ok()?)?
            .ty
            .clone()
    }

    /// The `($name)` `zend_argument_error_variadic` and
    /// `zend_param_must_be_nullable` write after `#N`, with its leading
    /// space, and nothing where the parameter has no name.
    fn argument_name_suffix(&self, position: u32) -> String {
        self.active_native_arg_name(position)
            .map(|name| format!(" (${name})"))
            .unwrap_or_default()
    }

    /// `zend_argument_error_variadic`'s message:
    /// `{fn}(): Argument #N ($name) must {tail}`.
    pub(crate) fn argument_message(&self, position: u32, tail: &str) -> String {
        format!(
            "{}(): Argument #{position}{} must {tail}",
            self.active_native(),
            self.argument_name_suffix(position)
        )
    }

    /// `zend_param_must_be_nullable`'s message:
    /// `{fn}(): Passing null to parameter #N ($name) of type {type} is
    /// deprecated`, one sentence, built as php builds it.
    pub(crate) fn null_parameter_message(&self, position: u32, type_name: &str) -> String {
        format!(
            "{}(): Passing null to parameter #{position}{} of type {type_name} is deprecated",
            self.active_native(),
            self.argument_name_suffix(position)
        )
    }

    pub(crate) fn precision(&self) -> i32 {
        self.ini_number(b"precision", 14)
    }

    /// `serialize_precision`, what `var_dump`, `var_export`, `serialize`
    /// and `json_encode` print a float at; -1, its default, asks for the
    /// shortest text that reads back as the same double.
    pub(crate) fn serialize_precision(&self) -> i32 {
        self.ini_number(b"serialize_precision", -1)
    }

    /// `max_execution_time`, the seconds the engine's execution timer is
    /// armed for; zero, php's CLI default, arms none.
    pub(crate) fn max_execution_time(&self) -> i64 {
        i64::from(self.ini_number(b"max_execution_time", 0))
    }

    /// Whether a handler left a throwable pending, php's `EG(exception)`:
    /// the test the loop performs after a `throw`-effect op and after a
    /// `Flow::Resume` (`jit-design.md` §2.2).
    pub(crate) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The pending throwable as a view, no owner taken: what `CATCH`
    /// tests its class against and what the unwinder chains onto.
    pub(crate) fn pending(&self) -> Option<PhpObject> {
        self.pending
    }

    /// The pending throwable out, the slot left empty and its one owner
    /// the caller's: php's `EG(exception) = NULL` beside the object it
    /// took, which is what a matching `CATCH` and an unwind into a
    /// `finally` block do with it.
    pub(crate) fn take_pending(&mut self) -> Option<PhpObject> {
        self.pending.take()
    }

    fn ini_number(&self, name: &[u8], default: i32) -> i32 {
        self.ini_get(name)
            .and_then(|value| String::from_utf8_lossy(value).parse().ok())
            .unwrap_or(default)
    }

    pub(crate) fn caller_strict_types(&self) -> bool {
        self.strict_types
    }

    /// `zend_is_callable_ex` (`Z/Zend/zend_API.c:4250`) without the
    /// error text: the callee a value names, or none.
    pub(crate) fn callable_of(&self, value: &Value) -> Option<Callable> {
        self.callable_checked(value).ok()
    }

    /// `zend_is_callable_at_frame` (`Z/Zend/zend_API.c:4139`) with its
    /// `*error`: a function name, a `Class::method` string, a
    /// `[$object, 'method']` or `['Class', 'method']` pair, or a
    /// `Closure`, resolved to the callee, the scope and the receiver a
    /// call takes, and named as `zend_get_callable_name_ex` names it.
    /// The `Err` is the sentence php's `*error` carries, which is what
    /// a native's `must be a valid callback` ends with.
    ///
    /// What php resolves off the frame it looks from, this does not:
    /// `self`, `parent` and `static` as the class half, the `__call`
    /// and `__callStatic` trampolines, and a method's visibility
    /// against the calling scope, so a non-public method is callable
    /// here where php refuses it.
    pub(crate) fn callable_checked(&self, value: &Value) -> Result<Callable, String> {
        // `case IS_REFERENCE: goto again`.
        let held = &value.deref();
        if let Some(object) = held.as_object() {
            // `case IS_OBJECT`: the `get_closure` handler, which only a
            // `Closure` has here, then `Class::__invoke`.
            if object.closure(self).is_some() {
                let name = self.declared_name(self.closure_callee(*object));
                return Ok(Callable::of(
                    *held,
                    name.as_bytes(),
                    Target::Closure(*object),
                ));
            }
            let class = object.class();
            return self.callable_method(*held, class, b"__invoke", Some(*object));
        }
        if let Some(array) = held.as_array() {
            return self.callable_pair(*held, *array);
        }
        let Some(name) = held.as_str_bytes() else {
            return Err(String::from("no array or string given"));
        };
        // `zend_is_callable_check_func`: the function table under the
        // name with a leading `\` skipped, and then the compound name
        // the last `::` splits.
        let bare = name.strip_prefix(b"\\").unwrap_or(name);
        if let Some(function) = self.function_named(bare) {
            return Ok(Callable::of(*held, name, Target::Function(function)));
        }
        let Some(at) = bare.windows(2).rposition(|pair| pair == b"::") else {
            return Err(format!(
                "function \"{}\" not found or invalid function name",
                String::from_utf8_lossy(name)
            ));
        };
        if at == 0 {
            return Err(String::from("invalid function name"));
        }
        let class = self.callable_class(&bare[..at])?;
        self.callable_method(*held, class, &bare[at + 2..], None)
    }

    /// `case IS_ARRAY` of `zend_is_callable_at_frame`: exactly two
    /// members at 0 and 1, the second a method name and the first the
    /// object or the class name it is looked up on.
    fn callable_pair(&self, value: Value, array: PhpArray) -> Result<Callable, String> {
        if array.len() != 2 {
            return Err(String::from("array callback must have exactly two members"));
        }
        let (Some(first), Some(second)) = (array.get(&Key::Int(0)), array.get(&Key::Int(1))) else {
            return Err(String::from(
                "array callback has to contain indices 0 and 1",
            ));
        };
        let (first, second) = (first.deref(), second.deref());
        if let Some(object) = first.as_object() {
            let Some(method) = second.as_str_bytes() else {
                return Err(String::from("second array member is not a valid method"));
            };
            let class = object.class();
            return self.callable_method(value, class, method, Some(*object));
        }
        let Some(name) = first.as_str_bytes() else {
            return Err(String::from(
                "first array member is not a valid class name or object",
            ));
        };
        let Some(method) = second.as_str_bytes() else {
            return Err(String::from("second array member is not a valid method"));
        };
        let class = self.callable_class(name)?;
        self.callable_method(value, class, method, None)
    }

    /// `zend_is_callable_check_class`'s last branch, `zend_lookup_class`
    /// over the class half of a callable.
    fn callable_class(&self, name: &[u8]) -> Result<ClassId, String> {
        self.class_named(name.strip_prefix(b"\\").unwrap_or(name))
            .ok_or_else(|| format!("class \"{}\" not found", String::from_utf8_lossy(name)))
    }

    /// The method half of `zend_is_callable_check_func`: the entry
    /// under the lowered name, php's two refusals for an abstract
    /// method and for an instance method named on the class with no
    /// receiver, and `zend_create_member_string` for the name the
    /// callable carries.
    fn callable_method(
        &self,
        value: Value,
        class: ClassId,
        method: &[u8],
        this: Option<PhpObject>,
    ) -> Result<Callable, String> {
        let spelled = format!(
            "{}::{}",
            self.class_name(class),
            String::from_utf8_lossy(method)
        );
        let Some(function) = self.method_named(class, &method.to_ascii_lowercase()) else {
            return Err(format!(
                "class {} does not have a method \"{}\"",
                self.class_name(class),
                String::from_utf8_lossy(method)
            ));
        };
        let decl = self.method_decl(function);
        if decl.is_some_and(|decl| decl.is_abstract) {
            return Err(format!("cannot call abstract method {spelled}()"));
        }
        // `fcc->object = NULL` for a static method, whatever it was
        // named on, and php's refusal for a non-static one named
        // statically.
        let this = this.filter(|_| decl.is_none_or(|decl| decl.is_instance));
        if this.is_none() && decl.is_some_and(|decl| decl.is_instance) {
            return Err(format!(
                "non-static method {spelled}() cannot be called statically"
            ));
        }
        Ok(Callable::of(
            value,
            spelled.as_bytes(),
            Target::Method {
                function,
                scope: class,
                this,
            },
        ))
    }

    /// `zend_get_closure_method_def`: the function a `Closure` object's
    /// payload holds, [`Function::PASS`] for an object that is none.
    fn closure_callee(&self, object: PhpObject) -> Function {
        object
            .closure(self)
            .map_or(Function::PASS, |closure| closure.function)
    }

    pub(crate) fn class_named(&self, name: &[u8]) -> Option<ClassId> {
        let wanted = name.to_ascii_lowercase();
        self.shared
            .classes
            .iter()
            .map(|decl| &decl.name)
            .chain(
                self.declared
                    .iter()
                    .filter_map(|(unit, at)| unit.classes.get(usize::try_from(*at).ok()?))
                    .map(|decl| &decl.name),
            )
            .position(|name| name.0.to_ascii_lowercase() == wanted)
            .and_then(|i| u32::try_from(i).ok())
            .map(ClassId)
    }

    /// `do_bind_class` for every class the unit declares that php bound
    /// while the file compiled, before its main op array runs: a
    /// top-level class implementing no interface, with no parent or one
    /// the table already held, is early bound and the compiler emitted
    /// no opline for it, so the request's table takes it here. Every
    /// other class carries a `ZEND_DECLARE_CLASS` where it was written
    /// and is bound as the program serves ([`Request::bind_class`]), so
    /// the ones named by one of those oplines are passed over here. A
    /// name already declared is `zend_class_redeclaration_error`,
    /// `E_COMPILE_ERROR` in php's two wordings, the site of the earlier
    /// declaration named when it was a user class, and the request ends
    /// before the frame is pushed; php runs nothing of a file whose
    /// class fails to bind.
    pub(crate) fn declare_classes(&mut self, unit: &Arc<Unit>) -> bool {
        let at_run_time: Vec<Vec<u8>> = unit
            .main
            .oplines
            .iter()
            .filter(|op| op.opcode == Opcode::DeclareClass)
            .filter_map(|op| {
                unit.main
                    .literals
                    .get(usize::try_from(op.op1).ok()?)
                    .and_then(|value| value.as_str())
                    .map(|name| name.bytes().to_vec())
            })
            .collect();
        for (at, decl) in unit.classes.iter().enumerate() {
            let lowered = decl.name.0.to_ascii_lowercase();
            if at_run_time.contains(&lowered) {
                continue;
            }
            if let Some(existing) = self.class_named(&decl.name.0) {
                let message = self.redeclaration(existing, &decl.name.0);
                let file = decl.file.0.to_vec();
                self.end_in_compile_time(Level::CompileError, &message, &file, decl.line);
                return false;
            }
            let Ok(at) = u32::try_from(at) else {
                return false;
            };
            self.bind_declared(unit, at);
        }
        true
    }

    /// `zend_class_redeclaration_error`'s two wordings: the site of the
    /// earlier declaration named when it was a user class.
    fn redeclaration(&self, existing: ClassId, name: &[u8]) -> String {
        match self.class_decl(existing) {
            Some(earlier) if !earlier.file.0.is_empty() => format!(
                "Cannot redeclare class {} (previously declared in {}:{})",
                String::from_utf8_lossy(name),
                String::from_utf8_lossy(&earlier.file.0),
                earlier.line
            ),
            _ => format!("Cannot redeclare class {}", String::from_utf8_lossy(name)),
        }
    }

    /// The class at `at` of `unit` into the request's table, with its
    /// methods into the function table beside it, which is the whole of
    /// what binding a class is here: the parent chain is walked from the
    /// declaration on every lookup, so nothing is copied.
    fn bind_declared(&mut self, unit: &Arc<Unit>, at: u32) {
        self.declared.push((Arc::clone(unit), at));
        let methods = unit
            .classes
            .get(usize::try_from(at).unwrap_or(usize::MAX))
            .map_or(0, |decl| decl.methods.len());
        for method in 0..methods {
            self.user_functions.push(UserFunction {
                unit: Arc::clone(unit),
                body: exec::Body::Method {
                    class: at,
                    method: u32::try_from(method).unwrap_or(u32::MAX),
                },
            });
        }
    }

    /// `zend_bind_class` at a `ZEND_DECLARE_CLASS`, `zend_do_link_class`
    /// over the declaration the opline's lowered name finds in the unit:
    /// the parent resolved first and then each interface, which is the
    /// order php's messages come in. A name nothing declares is php's
    /// `Error`, `Class "P" not found` for a parent and `Interface "I"
    /// not found` for an interface; a parent that is an interface, an
    /// interface list naming something that is not one, and the same
    /// interface twice are `zend_do_inheritance`'s and
    /// `zend_do_implement_interface`'s `E_ERROR`, which ends the request
    /// where it stands.
    ///
    /// Linking copies nothing: an inherited constant, property or
    /// method is found by walking `parent` from the declaration, so what
    /// this adds is the class's own row and its methods.
    ///
    /// The bind stands where the class was written, so what it refuses
    /// is refused after what the file has already printed, which is
    /// what tells this apart from the early bind:
    ///
    /// ```php
    /// echo "before\n";
    /// class C implements Nope {}
    /// ```
    /// ```expect exit=255
    /// before
    ///
    /// Fatal error: Uncaught Error: Interface "Nope" not found in %s:3
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 3
    /// ```
    ///
    /// ```php
    /// class P {}
    /// class C implements P {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: C cannot implement P - it is not an interface in %s on line 3
    /// ```
    pub(crate) fn bind_class(&mut self, unit: &Arc<Unit>, lowered: &[u8]) -> Result<(), Thrown> {
        let Some(at) = unit
            .classes
            .iter()
            .position(|decl| decl.name.0.to_ascii_lowercase() == lowered)
        else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a DECLARE_CLASS names a class the unit does not declare",
            ));
        };
        let Some(decl) = unit.classes.get(at).cloned() else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a DECLARE_CLASS names a class the unit does not declare",
            ));
        };
        let name = String::from_utf8_lossy(&decl.name.0).into_owned();
        if let Some(existing) = self.class_named(&decl.name.0) {
            let message = self.redeclaration(existing, &decl.name.0);
            return Err(crate::api::raising::fatal(self, &message));
        }
        if let Some(parent) = &decl.parent {
            let Some(found) = self.class_named(&parent.0) else {
                return Err(crate::api::raising::throw(
                    self,
                    "Error",
                    &format!("Class \"{}\" not found", String::from_utf8_lossy(&parent.0)),
                ));
            };
            if self.class_decl(found).map(|p| p.kind) == Some(ClassKind::Interface) {
                let message = format!(
                    "Class {name} cannot extend interface {}",
                    self.class_name(found)
                );
                return Err(crate::api::raising::fatal(self, &message));
            }
        }
        let mut implemented: Vec<ClassId> = Vec::new();
        for interface in &decl.interfaces {
            let Some(found) = self.class_named(&interface.0) else {
                return Err(crate::api::raising::throw(
                    self,
                    "Error",
                    &format!(
                        "Interface \"{}\" not found",
                        String::from_utf8_lossy(&interface.0)
                    ),
                ));
            };
            if self.class_decl(found).map(|i| i.kind) != Some(ClassKind::Interface) {
                let message = format!(
                    "{name} cannot implement {} - it is not an interface",
                    self.class_name(found)
                );
                return Err(crate::api::raising::fatal(self, &message));
            }
            if implemented.contains(&found) {
                let message = format!(
                    "Class {name} cannot implement previously implemented interface {}",
                    self.class_name(found)
                );
                return Err(crate::api::raising::fatal(self, &message));
            }
            implemented.push(found);
        }
        let Ok(at) = u32::try_from(at) else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a unit declares more classes than can be numbered",
            ));
        };
        self.bind_declared(unit, at);
        Ok(())
    }

    /// `zend_hash_find(EG(function_table), lcname)`: the native of that
    /// name, or the user function a unit of this request declared.
    pub(crate) fn function_named(&self, name: &[u8]) -> Option<Function> {
        let wanted = name.to_ascii_lowercase();
        if let Some(i) = self
            .shared
            .functions
            .iter()
            .position(|(decl, _)| decl.name.0.to_ascii_lowercase() == wanted)
        {
            return u32::try_from(i).ok().map(Function);
        }
        let position = self.user_functions.iter().position(|user| {
            user_function_decl(user).is_some_and(|decl| decl.name.0.to_ascii_lowercase() == wanted)
        })?;
        self.user_function_id(position)
    }

    /// `zend_hash_add_ptr(CG(function_table), …)` for every function the
    /// unit declares, before its main op array runs: php binds a
    /// top-level function at compile time and the compiler emitted no
    /// opline for it, so the request's table takes it here. The compiler
    /// refused a name the natives have or the file declares twice, so
    /// nothing collides in a request that serves one unit.
    /// `do_bind_function` at a `ZEND_DECLARE_FUNCTION`
    /// (`Z/Zend/zend_compile.c:1287`): the op array the opline's `op2`
    /// found among the frame's `dynamic_func_defs` into the request's
    /// function table under the lowered name in `op1`, which is
    /// `zend_hash_add_ptr(EG(function_table), lcname, func)`. A name the
    /// table already holds is `do_bind_function_error` at `E_ERROR`,
    /// naming the new declaration and, where the earlier one was a user
    /// function with a body, its file and its first line.
    pub(crate) fn bind_function(
        &mut self,
        unit: &Arc<Unit>,
        at: u32,
        lowered: &[u8],
    ) -> Result<(), Thrown> {
        let Some((decl, _)) = unit.dynamic.get(usize::try_from(at).unwrap_or(usize::MAX)) else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a DECLARE_FUNCTION naming a definition the unit has not",
            ));
        };
        let name = String::from_utf8_lossy(&decl.name.0).into_owned();
        if let Some(earlier) = self.function_named(lowered) {
            let site = self
                .user_function(earlier)
                .and_then(user_function_decl)
                .map(|decl| {
                    format!(
                        " (previously declared in {}:{})",
                        String::from_utf8_lossy(&decl.file.0),
                        decl.line
                    )
                })
                .unwrap_or_default();
            let message = format!("Cannot redeclare function {name}(){site}");
            return Err(crate::api::raising::fatal(self, &message));
        }
        self.user_functions.push(UserFunction {
            unit: Arc::clone(unit),
            body: exec::Body::Dynamic(at),
        });
        Ok(())
    }

    /// The `Function` a closure's op array is reached by, made where
    /// the request has not named it yet: php copies the whole
    /// `zend_function` into every `Closure` it creates
    /// (`zend_create_closure_ex`'s `memcpy`), and an op array here is
    /// immutable and shared, so one name stands for the declaration
    /// and what differs between two closures of it — the receiver, the
    /// scope and the bound variables — is the object's payload. The
    /// name is no entry of the function table under a callable name: a
    /// closure is reached through its object and never by name.
    pub(crate) fn closure_function(&mut self, unit: &Arc<Unit>, at: u32) -> Option<Function> {
        unit.dynamic.get(usize::try_from(at).ok()?)?;
        let body = exec::Body::Dynamic(at);
        if let Some(position) = self
            .user_functions
            .iter()
            .position(|user| user.body == body && Arc::ptr_eq(&user.unit, unit))
        {
            return self.user_function_id(position);
        }
        self.user_functions.push(UserFunction {
            unit: Arc::clone(unit),
            body,
        });
        self.user_function_id(self.user_functions.len() - 1)
    }

    /// `EX(func)->common.scope` of a frame, the class whose body the op
    /// array it serves was written in: what `zend_create_closure` is
    /// handed as the scope of a closure declared there, and none in a
    /// function or the main op array. Read off the frame and not off
    /// [`Request::executed_scope`], which answers for the native being
    /// served where one is.
    pub(crate) fn frame_scope(&self, frame: FrameRef) -> Option<ClassId> {
        self.method_class(self.vm.at(frame)?.function?)
    }

    pub(crate) fn declare_functions(&mut self, unit: &Arc<Unit>) {
        for at in 0..unit.functions.len() {
            self.user_functions.push(UserFunction {
                unit: Arc::clone(unit),
                body: exec::Body::Function(u32::try_from(at).unwrap_or(u32::MAX)),
            });
        }
    }

    pub(crate) fn constant_named(&self, name: &[u8]) -> Option<Value> {
        self.constants
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, v)| *v)
            .or_else(|| self.shared.constant(name))
    }

    fn ini_parse_bytes_of(&self, name: &str) -> i64 {
        self.ini_get(name.as_bytes())
            .map_or(0, Request::ini_parse_bytes)
    }

    // -------------------------------------------------- output and headers

    /// Write body bytes, `php_output_write`: through the handler stack
    /// while one is active, and what passes it reaches the host, with
    /// the header block committed at the first byte that does
    /// (`crate::output`).
    pub fn write(&mut self, bytes: &[u8]) {
        crate::output::write(self, bytes);
    }

    /// `sapi_module.ub_write`: bytes past the `php_output` layer, the host's.
    pub(crate) fn ub_write(&mut self, bytes: &[u8]) {
        self.body.extend_from_slice(bytes);
    }

    /// The `php_output` layer, and the heap it lays its handlers down in.
    pub(crate) fn output_and_heap(&mut self) -> (&mut Output, &mut Heap) {
        (&mut self.output, &mut self.heap)
    }

    pub(crate) fn output(&self) -> &Output {
        &self.output
    }

    pub(crate) fn output_mut(&mut self) -> &mut Output {
        &mut self.output
    }

    /// `header_register_callback`'s callable, taken to be served once by
    /// `sapi_send_headers`, which unsets `SG(callback_func)` before the
    /// call so a second send finds none.
    pub(crate) fn take_header_callback(&mut self) -> Option<Callable> {
        self.header_callback.take()
    }

    /// Write to the host's error stream, bytes as given: php's
    /// `fputs(stderr)`, which the `display_errors=stderr` form takes and
    /// the `STDERR` stream constant will. The log is another channel,
    /// [`Request::error_log`]'s; whether the host puts the two on one fd
    /// is the host's, as it is php's CLI's (#216).
    pub fn write_stderr(&mut self, bytes: &[u8]) {
        self.aside.push(Aside::Stderr(bytes.to_vec()));
    }

    /// `flush()`, `sapi_flush`: the host may push what it has. The host
    /// is handed the body by `serve`, once, so there is nothing to push
    /// before then; the handler stack is `ob_flush`'s, not this.
    pub fn flush(&mut self) {}

    /// One `ob_*` operation on the handler stack, the `PHP_FUNCTION`
    /// bodies of `Z/main/output.c:1373-1591` over the layer in
    /// `crate::output`; the notices are theirs, raised through the
    /// native being served.
    pub fn output_buffer_op(&mut self, op: &OutputOp) -> Result<Value, Thrown> {
        crate::output::buffer_op(self, op)
    }

    /// Where the first byte reached the host, `php_output_get_start_filename`
    /// and `_lineno`, once one has.
    #[must_use]
    pub fn output_started_at(&self) -> Option<(Vec<u8>, u32)> {
        self.output.started_at()
    }

    /// `header()`, `sapi_header_op` under `SAPI_HEADER_ADD` or
    /// `SAPI_HEADER_REPLACE` (`crate::output`): the line taken, replacing
    /// the lines of its name when `replace` is set, with `response_code`
    /// set when it is not zero; once the block is sent, php's warning
    /// naming where the body started, and `false`. Under the CLI the
    /// host's `header_handler` takes no line, so the list stays empty
    /// and the status line and the response code are all a `header()`
    /// moves, as under php's.
    pub fn header_add(&mut self, line: &[u8], replace: bool, response_code: i64) -> bool {
        crate::output::header_add(self, line, replace, response_code)
    }

    /// `header_remove()`, `sapi_header_op` under `SAPI_HEADER_DELETE` or,
    /// with `None`, `SAPI_HEADER_DELETE_ALL`: the lines of that name
    /// dropped, or all of them; the warnings are `sapi_header_op`'s.
    pub fn header_remove(&mut self, name: Option<&[u8]>) -> bool {
        crate::output::header_remove(self, name)
    }

    /// `headers_list()`: `SG(sapi_headers).headers`, in order.
    #[must_use]
    pub fn headers_list(&self) -> Vec<Vec<u8>> {
        self.output.headers().to_vec()
    }

    /// `SG(sapi_headers).http_status_line`: the `HTTP/...` line a
    /// `header()` set, held until the block is sent.
    #[must_use]
    pub fn status_line(&self) -> Option<&[u8]> {
        self.output.status_line()
    }

    /// `headers_sent()`: `SG(headers_sent)`, and where the body started
    /// when it was a body byte that sent them.
    #[must_use]
    pub fn headers_sent(&self) -> Option<(Vec<u8>, u32)> {
        if self.output.headers_sent() {
            Some(self.output.started_at().unwrap_or_default())
        } else {
            None
        }
    }

    /// `header_register_callback()`.
    pub fn set_header_callback(&mut self, callback: Callable) {
        self.header_callback = Some(callback);
    }

    /// `SG(sapi_headers).http_response_code` as it stands: 200 under a
    /// web host and zero under the CLI, which `http_response_code()`
    /// answers `false` for.
    #[must_use]
    pub fn response_code(&self) -> i64 {
        self.output.response_code()
    }

    /// `http_response_code(int)`'s write, the code alone
    /// (`Z/ext/standard/head.c:399`); the checks before it are the
    /// native's.
    pub fn set_response_code(&mut self, code: i64) {
        self.output.set_response_code(code);
    }

    /// The request headers the host received, `getallheaders()`, as
    /// strings of the request's.
    pub fn request_headers(&mut self) -> Result<PhpArray, Thrown> {
        let mut array = PhpArray::new(self)?;
        let headers = self.description.headers.clone();
        for (name, value) in &headers {
            let key = Key::Str(PhpString::new(self, name)?);
            let value = Value::string(PhpString::new(self, value)?);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    // -------------------------------------------------- diagnostics

    /// `error_get_last()`: the last diagnostic as php's four-key array,
    /// its strings the request's.
    pub fn last_error(&mut self) -> Result<Option<PhpArray>, Thrown> {
        let Some((level, message, file, line)) = self.last_error.clone() else {
            return Ok(None);
        };
        let mut array = PhpArray::new(self)?;
        for (name, value) in [
            (&b"type"[..], Value::int(level.bit())),
            (
                b"message",
                Value::string(PhpString::new(self, message.as_bytes())?),
            ),
            (b"file", Value::string(PhpString::new(self, &file)?)),
            (b"line", Value::int(i64::from(line))),
        ] {
            let key = Key::Str(PhpString::new(self, name)?);
            array.set(self, key, value)?;
        }
        Ok(Some(array))
    }

    /// `error_clear_last()`.
    pub fn clear_last_error(&mut self) {
        self.last_error = None;
    }

    /// `error_log()` to the host's log: `php_log_err` with the message
    /// as given, one entry.
    pub fn error_log(&mut self, message: &[u8]) {
        self.log_err(message);
    }

    /// `error_reporting()` as it stands.
    #[must_use]
    pub fn error_reporting(&self) -> i64 {
        self.ini_get(b"error_reporting")
            .and_then(|v| String::from_utf8_lossy(v).parse().ok())
            .unwrap_or(32767)
    }

    /// `set_error_handler()`: the previous handler.
    pub fn set_error_handler(&mut self, handler: Option<Callable>, levels: i64) -> Option<Value> {
        let _ = levels;
        let previous = self.error_handlers.last().cloned().flatten();
        self.error_handlers.push(handler);
        previous.map(Callable::into_value)
    }

    /// `restore_error_handler()`.
    pub fn restore_error_handler(&mut self) -> bool {
        self.error_handlers.pop().is_some()
    }

    /// `set_exception_handler()`: the previous handler.
    pub fn set_exception_handler(&mut self, handler: Option<Callable>) -> Option<Value> {
        let previous = self.exception_handlers.last().cloned().flatten();
        self.exception_handlers.push(handler);
        previous.map(Callable::into_value)
    }

    /// `restore_exception_handler()`.
    pub fn restore_exception_handler(&mut self) -> bool {
        self.exception_handlers.pop().is_some()
    }

    /// `assert_options(ASSERT_CALLBACK)` as it stands.
    #[must_use]
    pub fn assert_callback(&self) -> Option<&Value> {
        self.assert_callback.as_ref()
    }

    /// Set the assertion callback.
    pub fn set_assert_callback(&mut self, callback: Option<Value>) {
        self.assert_callback = callback;
    }

    /// `debug_backtrace()` with php's options and limit; empty while no
    /// frame is live.
    pub fn backtrace(&mut self, options: i64, limit: i64) -> Result<PhpArray, Thrown> {
        let _ = (options, limit);
        PhpArray::new(self)
    }

    /// The file of the frame that called the native,
    /// `zend_get_executed_filename`.
    #[must_use]
    pub fn current_file(&self) -> Option<&[u8]> {
        self.description.script_name.as_deref()
    }

    /// `zend_get_executed_lineno`: the line the innermost frame stands
    /// on, which is the line a diagnostic and a backtrace name.
    #[must_use]
    pub fn current_line(&self) -> u32 {
        self.vm.executing_line()
    }

    /// `ZEND_NUM_ARGS()`: how many arguments the call being served was
    /// given, and zero outside a native's body.
    #[must_use]
    pub fn argument_count(&self) -> usize {
        self.vm
            .serving_call()
            .map_or(0, |call| call.arguments.len())
    }

    /// `ZEND_CALL_ARG(execute_data, position)`: the argument at
    /// `position`, one-based as php counts them and as `Argument #N`
    /// names them, read off the call frame the native is being served
    /// from; none past what the call was given. What is read is the
    /// value as passed, a reference not followed, as `Z_PARAM_*` reads
    /// its slot before coercing it.
    #[must_use]
    pub fn argument(&self, position: u32) -> Option<Value> {
        let call = self.vm.serving_call()?;
        let index = usize::try_from(position.checked_sub(1)?).ok()?;
        call.arguments.get(index).copied()
    }

    /// `zend_parse_arg_*_weak` writing what it coerced back into the
    /// argument's zval: the call frame gives up what it held at
    /// `position` and owns `value` from here, so the native reads a view
    /// and the frame frees the string with the call. A position past
    /// what the call was given writes nothing. A frameless call's
    /// argument is the opline's operand, which php's `Z_FLF_PARAM_STR`
    /// never writes: it converts a temporary of the body's and frees it
    /// at the body's end, so the frame keeps `value` beside the argument
    /// and a trace taken after the coercion prints the operand as passed.
    pub(crate) fn replace_argument(&mut self, position: u32, value: Value) {
        let Some(index) = position
            .checked_sub(1)
            .and_then(|at| usize::try_from(at).ok())
        else {
            return;
        };
        let Some(call) = self.vm.serving_call_mut() else {
            return;
        };
        if call.frameless {
            call.coerced.push(value);
            return;
        }
        let Some(previous) = call
            .arguments
            .get_mut(index)
            .map(|slot| std::mem::replace(slot, value))
        else {
            return;
        };
        self.release(previous);
    }

    /// The arguments of the frame `depth` levels up, `func_get_args` from
    /// outside.
    #[must_use]
    pub fn frame_args(&self, depth: u32) -> Option<Vec<Value>> {
        let _ = depth;
        None
    }

    /// The variables of the frame `depth` levels up, `get_defined_vars`.
    #[must_use]
    pub fn frame_vars(&self, depth: u32) -> Option<PhpArray> {
        let _ = depth;
        None
    }

    /// `zend_get_executed_scope`, the class scope of the native being
    /// served: a method's own class (`func->common.scope`), and none for
    /// a function or outside a native's body.
    #[must_use]
    pub fn scope(&self) -> Option<ClassId> {
        self.executed_scope()
    }

    /// `zend_get_called_scope`, the called scope of the native being
    /// served, `static`: the receiver's class for an instance call and
    /// the class named for a call on the class; none for a function or
    /// outside a native's body.
    #[must_use]
    pub fn called_scope(&self) -> Option<ClassId> {
        if let Some(call) = self.vm.serving_call() {
            return call.called_scope;
        }
        self.vm.innermost()?.called_scope
    }

    /// `ZEND_THIS`, `getThis()`: the receiver of the method being
    /// served, a name the frame holds an owner of while the call
    /// stands; none for a call on the class, a function, or outside a
    /// native's body.
    #[must_use]
    pub fn this(&self) -> Option<PhpObject> {
        if let Some(call) = self.vm.serving_call() {
            return call.this;
        }
        self.vm.innermost()?.this
    }

    // -------------------------------------------------- symbols

    /// The declaration of a linked class.
    #[must_use]
    pub fn class_decl(&self, class: ClassId) -> Option<&ClassDecl> {
        let at = usize::try_from(class.0).ok()?;
        match at.checked_sub(self.shared.classes.len()) {
            None => self.shared.classes.get(at),
            Some(declared) => {
                let (unit, index) = self.declared.get(declared)?;
                unit.classes.get(usize::try_from(*index).ok()?)
            }
        }
    }

    /// `ZEND_FLF_FUNC(opline)`: the native and the arity the engine's
    /// frameless table holds at `at`, a `FRAMELESS_ICALL_*`'s
    /// `extended_value`; none past the table, which a unit this engine
    /// compiled never carries.
    pub(crate) fn frameless_entry(&self, at: u32) -> Option<FramelessEntry> {
        self.shared.frameless.get(at as usize).copied()
    }

    /// `zend_flf_count`: how many rows the frameless table has.
    pub(crate) fn frameless_count(&self) -> usize {
        self.shared.frameless.len()
    }

    /// The declaration of a linked function.
    #[must_use]
    pub fn function_decl(&self, function: Function) -> Option<&FunctionDecl> {
        let at = usize::try_from(function.0).ok()?;
        if at < self.shared.functions.len() {
            return self.shared.functions.get(at).map(|(decl, _)| decl);
        }
        if let Some(decl) = self.user_function(function).and_then(user_function_decl) {
            return Some(decl);
        }
        self.method_decl(function).map(|method| &method.function)
    }

    /// `get_declared_classes()`, in declaration order.
    #[must_use]
    pub fn declared_classes(&self) -> Vec<ClassId> {
        let count = self.shared.classes.len() + self.declared.len();
        (0..u32::try_from(count).unwrap_or(u32::MAX))
            .map(ClassId)
            .collect()
    }

    /// `get_defined_functions()`: `internal` and `user` lists, the names
    /// strings of the request's.
    pub fn defined_functions(&mut self) -> Result<PhpArray, Thrown> {
        let mut internal = PhpArray::new(self)?;
        let shared = Arc::clone(&self.shared);
        for (decl, _) in &shared.functions {
            let name = Value::string(PhpString::new(self, &decl.name.0)?);
            internal.append(self, name)?;
        }
        let mut array = PhpArray::new(self)?;
        let key = Key::Str(PhpString::new(self, b"internal")?);
        array.set(self, key, Value::array(internal))?;
        let key = Key::Str(PhpString::new(self, b"user")?);
        let user = PhpArray::new(self)?;
        array.set(self, key, Value::array(user))?;
        Ok(array)
    }

    /// `class_alias()`.
    pub fn class_alias(&mut self, class: ClassId, alias: &[u8]) -> Result<(), Thrown> {
        let _ = (class, alias);
        Ok(())
    }

    /// `get_defined_constants()`: what `define()` declared in this
    /// request, each name a string of the request's.
    pub fn constants(&mut self) -> Result<PhpArray, Thrown> {
        let mut array = PhpArray::new(self)?;
        let constants = self.constants.clone();
        for (name, value) in constants {
            let key = Key::Str(PhpString::new(self, &name)?);
            self.addref(&value);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// `define()`: `false` when the constant exists.
    pub fn define_constant(&mut self, name: &[u8], value: Value) -> Result<bool, Thrown> {
        if self.constant_named(name).is_some() {
            return Ok(false);
        }
        self.constants.push((name.to_vec(), value));
        Ok(true)
    }

    /// `spl_autoload_register()`.
    pub fn autoload_register(&mut self, loader: Callable, prepend: bool) -> Result<(), Thrown> {
        if prepend {
            self.autoloaders.insert(0, loader);
        } else {
            self.autoloaders.push(loader);
        }
        Ok(())
    }

    /// `spl_autoload_unregister()`.
    pub fn autoload_unregister(&mut self, loader: &Callable) -> bool {
        let before = self.autoloaders.len();
        self.autoloaders.retain(|l| l != loader);
        self.autoloaders.len() != before
    }

    /// `spl_autoload_functions()`.
    #[must_use]
    pub fn autoload_functions(&self) -> Vec<Value> {
        self.autoloaders
            .iter()
            .cloned()
            .map(Callable::into_value)
            .collect()
    }

    /// `get_included_files()`: the paths, as `EG(included_files)` holds
    /// them.
    #[must_use]
    pub fn included_files(&self) -> Vec<Vec<u8>> {
        self.description.script_name.iter().cloned().collect()
    }

    /// `get_loaded_extensions()`: the bundle in `MINIT` order, after
    /// `Core`, each the name its `zend_module_entry` carries.
    #[must_use]
    pub fn loaded_extensions(&self) -> Vec<String> {
        std::iter::once(String::from("Core"))
            .chain(self.shared.extensions.iter().map(|e| e.name.clone()))
            .collect()
    }

    /// `zend_hash_str_find_ptr(&module_registry, lcname)`: the linked
    /// extension named, php's table being keyed by the lowered name;
    /// none for a name the bundle has not, and none for `Core` until
    /// the engine's own natives are an `Extension` value.
    #[must_use]
    pub fn extension_named(&self, name: &[u8]) -> Option<ExtensionId> {
        let wanted = name.to_ascii_lowercase();
        self.shared
            .extensions
            .iter()
            .position(|e| e.name.as_bytes().to_ascii_lowercase() == wanted)
            .and_then(|i| u32::try_from(i).ok())
            .map(ExtensionId)
    }

    /// `module->name`: the name an extension registered under, as its
    /// `zend_module_entry` spells it.
    #[must_use]
    pub fn extension_name(&self, extension: ExtensionId) -> Option<&str> {
        self.extension_of(extension).map(|e| e.name.as_str())
    }

    /// `module->module_number`, `zend_next_free_module` at registration:
    /// `Core` takes zero at `zend_startup` and each extension the next
    /// number in startup order, so an extension's is its position in
    /// the bundle plus one.
    #[must_use]
    pub fn extension_number(&self, extension: ExtensionId) -> u32 {
        extension.0.saturating_add(1)
    }

    /// The natives an extension declared, in the order it declared
    /// them: what `CG(function_table)` holds with
    /// `internal_function.module` naming it.
    #[must_use]
    pub fn extension_functions(&self, extension: ExtensionId) -> Vec<Function> {
        let Some(at) = usize::try_from(extension.0).ok() else {
            return Vec::new();
        };
        let start = self.extension_functions_start(at);
        let count = self
            .extension_of(extension)
            .map_or(0, |e| e.functions.len());
        (start..start.saturating_add(count))
            .filter_map(|i| u32::try_from(i).ok())
            .map(Function)
            .collect()
    }

    /// `internal_function.module`: the extension that declared a native
    /// function; none for a native the engine declares itself, for a
    /// method and for a user function.
    #[must_use]
    pub fn function_extension(&self, function: Function) -> Option<ExtensionId> {
        let at = usize::try_from(function.0).ok()?;
        (0..self.shared.extensions.len())
            .find(|&i| {
                let start = self.extension_functions_start(i);
                let count = self.shared.extensions[i].functions.len();
                (start..start.saturating_add(count)).contains(&at)
            })
            .and_then(|i| u32::try_from(i).ok())
            .map(ExtensionId)
    }

    /// `ce->info.internal.module`: the extension that declared a native
    /// class, `ReflectionClass::getExtension` reads it; none for a class
    /// Zend declares itself, whose `zend_module_entry` php has as `Core` and this
    /// engine as no `Extension` value yet, and none for a user class.
    #[must_use]
    pub fn class_extension(&self, class: ClassId) -> Option<ExtensionId> {
        let at = usize::try_from(class.0).ok()?;
        let zend = self
            .shared
            .classes
            .len()
            .saturating_sub(self.shared.extensions.iter().map(|e| e.classes.len()).sum());
        let mut start = zend;
        for (i, extension) in self.shared.extensions.iter().enumerate() {
            let end = start.saturating_add(extension.classes.len());
            if (start..end).contains(&at) {
                return u32::try_from(i).ok().map(ExtensionId);
            }
            start = end;
        }
        None
    }

    /// `ce->info.internal.module->name` as `_class_string` and
    /// `_function_string` print it after `<internal:`: the extension's
    /// name for a class an extension declared, `Core` for one Zend
    /// declares itself, whose `zend_module_entry` php has under that
    /// name and this engine as no `Extension` value yet (#320), and
    /// none for a user class.
    #[must_use]
    pub fn class_extension_name(&self, class: ClassId) -> Option<&str> {
        if let Some(extension) = self.class_extension(class) {
            return self.extension_name(extension);
        }
        let at = usize::try_from(class.0).ok()?;
        (at < self.shared.classes.len()).then_some("Core")
    }

    /// `ZEND_CONSTANT_MODULE_NUMBER(constant) == module->module_number`
    /// over `EG(zend_constants)`: the names of the constants an
    /// extension declared through `ExtensionBuilder::constants`, in the
    /// order it declared them, each one `api::call::constant` reads the
    /// value of; empty for an extension that declares none. The table
    /// holds the engine's own constants first, as
    /// `zend_register_standard_constants` runs before any `MINIT`, then
    /// each extension's in bundle order (`Engine::build`), the layout
    /// [`Request::extension_functions`] reads too.
    #[must_use]
    pub fn extension_constants(&self, extension: ExtensionId) -> Vec<Name> {
        let Some(at) = usize::try_from(extension.0).ok() else {
            return Vec::new();
        };
        let all: usize = self
            .shared
            .extensions
            .iter()
            .map(|e| e.constants.len())
            .sum();
        let own = self.shared.declared().constants.len().saturating_sub(all);
        let start: usize = self
            .shared
            .extensions
            .iter()
            .take(at)
            .map(|e| e.constants.len())
            .fold(own, usize::saturating_add);
        let count = self
            .extension_of(extension)
            .map_or(0, |e| e.constants.len());
        self.shared
            .declared()
            .constants
            .iter()
            .skip(start)
            .take(count)
            .map(|decl| decl.name.clone())
            .collect()
    }

    /// `module->deps`: the rows an extension declared through
    /// `ExtensionBuilder::dependencies`, in their order; the empty
    /// table where php's pointer is null, which is every extension that
    /// declares none.
    #[must_use]
    pub fn extension_dependencies(&self, extension: ExtensionId) -> Vec<Dependency> {
        self.extension_of(extension)
            .map(|e| e.dependencies.clone())
            .unwrap_or_default()
    }

    /// `php_info_print_module` as the CLI prints it, `phpinfo_as_text`
    /// (`Z/ext/standard/info.c:133`): the name as a one-column table,
    /// then the extension's `MINFO`; for an extension that declared
    /// none, a `Version` row and `DISPLAY_INI_ENTRIES`, its ini entries
    /// under `Directive => Local Value => Master Value`
    /// (`Z/main/php_ini.c:115`), the master being the engine's default
    /// and an empty value `no value`. Every bundled extension has a
    /// version, `PHP_VERSION`, so php's other branch, the bare name, is
    /// never taken. What `ReflectionExtension::info` and `php --ri` print.
    pub fn extension_info(&mut self, extension: ExtensionId) {
        let Some(module) = self.extension_of(extension).cloned() else {
            return;
        };
        self.info_table(&[&[module.name.as_str()]]);
        if let Some(info) = module.info {
            info(self);
            return;
        }
        self.info_table(&[&["Version", PHP_VERSION]]);
        if module.ini.is_empty() {
            return;
        }
        let no_value = || String::from("no value");
        let shown = |value: Option<&[u8]>| {
            value
                .filter(|v| !v.is_empty())
                .map_or_else(no_value, |v| String::from_utf8_lossy(v).into_owned())
        };
        let rows: Vec<[String; 3]> = module
            .ini
            .iter()
            .map(|entry| {
                [
                    String::from_utf8_lossy(&entry.name.0).into_owned(),
                    shown(self.ini_get(&entry.name.0)),
                    shown(self.shared.ini_default(&entry.name.0)),
                ]
            })
            .collect();
        let mut table: Vec<&[&str]> = vec![&["Directive", "Local Value", "Master Value"]];
        let cells: Vec<[&str; 3]> = rows
            .iter()
            .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
            .collect();
        table.extend(cells.iter().map(<[&str; 3]>::as_slice));
        self.info_table(&table);
    }

    /// `php_info_print_table_start` and one `php_info_print_table_row`
    /// per row as the CLI prints them, `phpinfo_as_text`
    /// (`Z/ext/standard/info.c:1062-1220`): a blank line, then each
    /// row's cells with ` => ` between and a newline after, an empty
    /// cell one space with no ` => ` after it, as php's loop has it.
    /// A header row prints the same way in text, so an extension's
    /// `MINFO` writes its whole table through this.
    pub fn info_table(&mut self, rows: &[&[&str]]) {
        let mut out = String::from("\n");
        for row in rows {
            let last = row.len().saturating_sub(1);
            for (i, cell) in row.iter().enumerate() {
                if cell.is_empty() {
                    out.push(' ');
                } else {
                    out.push_str(cell);
                    if i < last {
                        out.push_str(" => ");
                    }
                }
                if i == last {
                    out.push('\n');
                }
            }
        }
        self.write(out.as_bytes());
    }

    fn extension_of(&self, extension: ExtensionId) -> Option<&Extension> {
        self.shared
            .extensions
            .get(usize::try_from(extension.0).ok()?)
    }

    /// Where extension `at`'s functions begin in the linked table: the
    /// engine's own natives come first, then each extension's in bundle
    /// order (`Engine::build`).
    fn extension_functions_start(&self, at: usize) -> usize {
        let all: usize = self
            .shared
            .extensions
            .iter()
            .map(|e| e.functions.len())
            .sum();
        let own = self.shared.functions.len().saturating_sub(all);
        self.shared
            .extensions
            .iter()
            .take(at)
            .map(|e| e.functions.len())
            .fold(own, usize::saturating_add)
    }

    // -------------------------------------------------- state

    /// The request state of the calling extension, `T` being the type it
    /// declared with `request_state::<T>()` (ADR-0005). A type no bundled
    /// extension declared is built on first touch here, where the design
    /// names a bug; the linker's slot table replaces this scan.
    pub fn state<T: Default + Send + 'static>(&mut self) -> &mut T {
        let wanted = TypeId::of::<T>();
        let at = if let Some(at) = self.states.iter().position(|(id, _)| *id == wanted) {
            at
        } else {
            self.states.push((wanted, Box::new(T::default())));
            self.states.len() - 1
        };
        if self.states[at].1.downcast_ref::<T>().is_none() {
            self.states[at].1 = Box::new(T::default());
        }
        match self.states[at].1.downcast_mut::<T>() {
            Some(state) => state,
            // Written as a `T` on the line above; this arm is the type
            // system's, not a path a request takes.
            None => Box::leak(Box::new(T::default())),
        }
    }

    /// The engine state of the calling extension, `E` being the type its
    /// `engine_state` built (ADR-0005). `None` for a type no extension
    /// built, where the design names a bug.
    #[must_use]
    pub fn engine<E: Send + Sync + 'static>(&self) -> Option<&E> {
        self.shared
            .engine_states
            .iter()
            .find_map(|state| state.downcast_ref::<E>())
    }

    /// The request's resource types, for an extension registering one.
    pub fn resources(&mut self) -> &mut Resources {
        &mut self.resources
    }

    /// `zend_hash_index_find(&EG(regular_list), id)`: the resource with
    /// `id`, while its entry stands, closed or not.
    #[must_use]
    pub fn resource(&self, id: i64) -> Option<Resource> {
        self.heap.resource_at(id).map(Resource::of)
    }

    /// This request's interrupt (design §5.8): what the loop tests at a
    /// back-edge, at a frame's activation and after every
    /// `Flow::Resume`, and what the engine's execution timer and signal
    /// delivery raise. A `tier.lock` item, because compiled code tests
    /// the same atomic once per loop iteration through the tier's
    /// `ExitCtx` (`jit-design.md` §4.3).
    #[must_use]
    pub fn interrupt(&self) -> &Interrupt {
        &self.interrupt
    }

    // -------------------------------------------------- ini

    /// `ini_get()`: the request's override, else the engine's default.
    #[must_use]
    pub fn ini_get(&self, name: &[u8]) -> Option<&[u8]> {
        self.ini
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, v)| v.as_slice())
            .or_else(|| self.shared.ini_default(name))
    }

    /// `ini_set()`: the previous value, or `None` for an entry that does
    /// not exist, may not change at runtime, or whose `on_modify` refuses
    /// the value ([`Request::ini_verdict`]). An entry with an on-modify
    /// handler runs it here: `max_execution_time` re-arms the execution
    /// timer (`OnUpdateTimeout`, `Z/main/main.c:467`).
    ///
    /// The two entries of the table without `PHP_INI_USER` are
    /// `xmlrpc_errors`, `PHP_INI_SYSTEM`, and `short_open_tag`,
    /// `PHP_INI_SYSTEM|PHP_INI_PERDIR`, which `zend_alter_ini_entry_ex`
    /// refuses at `PHP_INI_STAGE_RUNTIME` as it refuses every entry whose
    /// `modifiable` lacks the stage; the rest of the table is `PHP_INI_ALL`.
    ///
    /// ```php
    /// var_dump(ini_set('xmlrpc_errors', '1'), ini_set('short_open_tag', '1'));
    /// echo NAN;
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    ///
    /// Warning: unexpected NAN value was coerced to string in %s on line 3
    /// NAN
    /// ```
    pub fn ini_set(&mut self, name: &[u8], value: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
        let previous = self.ini_get(name).map(<[u8]>::to_vec);
        if previous.is_none() || !Request::ini_modifiable(name) {
            return Ok(None);
        }
        if Request::ini_verdict(name, value) == IniVerdict::Rejected {
            return Ok(None);
        }
        self.ini.retain(|(n, _)| n.as_slice() != name);
        self.ini.push((name.to_vec(), value.to_vec()));
        match name {
            b"max_execution_time" => self.arm_timer(),
            b"memory_limit" => self.set_memory_limit(),
            _ => {}
        }
        Ok(previous)
    }

    /// Whether an entry's `modifiable` carries `PHP_INI_USER`, what
    /// `ini_set()` asks: every entry of the table is `PHP_INI_ALL` but
    /// `xmlrpc_errors`, `PHP_INI_SYSTEM` (`Z/main/main.c:806`), and
    /// `short_open_tag`, `PHP_INI_SYSTEM|PHP_INI_PERDIR` (`Z/main/main.c:820`).
    fn ini_modifiable(name: &[u8]) -> bool {
        !matches!(name, b"xmlrpc_errors" | b"short_open_tag")
    }

    /// What a core entry's `on_modify` says of a new value before
    /// anything is written, `zend_alter_ini_entry_ex` asking it first
    /// (`Z/Zend/zend_ini.c:342`): `zend.exception_string_param_max_len`
    /// is `OnSetExceptionStringParamMaxLen` (`Z/Zend/zend.c:175-184`),
    /// which reads the text with `ZEND_ATOL` and refuses a number outside
    /// `0..=1000000`; the rest of the table accepts anything. A refusal
    /// keeps the entry as it was and `ini_set()` answers `false`.
    ///
    /// ```php
    /// var_dump(ini_set("zend.exception_string_param_max_len", "-1"), ini_set("zend.exception_string_param_max_len", "1000001"), ini_set("zend.exception_string_param_max_len", "3"), ini_get("zend.exception_string_param_max_len"));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// string(2) "15"
    /// string(1) "3"
    /// ```
    ///
    /// `ZEND_ATOL` is `atoll`, so text that is not a number is zero and
    /// accepted, and the entry keeps the text it was given.
    ///
    /// ```php
    /// var_dump(ini_set("zend.exception_string_param_max_len", "abc"), ini_get("zend.exception_string_param_max_len"), ini_set("zend.exception_string_param_max_len", "99999999999999999999"));
    /// ```
    /// ```expect
    /// string(2) "15"
    /// string(3) "abc"
    /// bool(false)
    /// ```
    fn ini_verdict(name: &[u8], value: &[u8]) -> IniVerdict {
        match name {
            b"zend.exception_string_param_max_len"
                if !(0..=1_000_000).contains(&ini_atol(value)) =>
            {
                IniVerdict::Rejected
            }
            _ => IniVerdict::Accepted,
        }
    }

    /// `ini_get_all()`: every entry with its current value, as strings
    /// of the request's.
    pub fn ini_entries(&mut self) -> Result<PhpArray, Thrown> {
        let mut array = PhpArray::new(self)?;
        let shared = Arc::clone(&self.shared);
        for (name, default) in shared.ini_defaults() {
            let value = self.ini_get(name).unwrap_or(default).to_vec();
            let key = Key::Str(PhpString::new(self, name)?);
            let value = Value::string(PhpString::new(self, &value)?);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// `zend_ini_parse_bool`: `1`, `on`, `yes`, `true` are true.
    #[must_use]
    pub fn ini_parse_bool(value: &[u8]) -> bool {
        let lower = value.to_ascii_lowercase();
        matches!(lower.as_slice(), b"1" | b"on" | b"yes" | b"true")
            || lower.iter().all(u8::is_ascii_digit)
                && String::from_utf8_lossy(&lower)
                    .parse::<i64>()
                    .is_ok_and(|n| n != 0)
    }

    /// `zend_ini_parse_quantity`: a byte count with an optional `K`, `M`
    /// or `G` suffix; `-1` and other text parse as php's `atoi` would.
    #[must_use]
    pub fn ini_parse_bytes(value: &[u8]) -> i64 {
        let text = String::from_utf8_lossy(value);
        let text = text.trim();
        let (digits, shift) = match text.chars().last().map(|c| c.to_ascii_lowercase()) {
            Some('k') => (&text[..text.len() - 1], 10),
            Some('m') => (&text[..text.len() - 1], 20),
            Some('g') => (&text[..text.len() - 1], 30),
            _ => (text, 0),
        };
        let n: i64 = digits.trim().parse().unwrap_or(0);
        n.checked_shl(shift).unwrap_or(i64::MAX)
    }

    // -------------------------------------------------- environment

    /// `getenv(name)`: the bytes the host's environment holds.
    #[must_use]
    pub fn env(&self, name: &[u8]) -> Option<&[u8]> {
        self.description
            .env
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, v)| v.as_slice())
    }

    /// `getenv()` with no name: every pair.
    #[must_use]
    pub fn env_pairs(&self) -> &[(Vec<u8>, Vec<u8>)] {
        &self.description.env
    }

    /// `putenv()`: set, or unset with `None`.
    pub fn set_env(&mut self, name: &[u8], value: Option<&[u8]>) -> bool {
        self.description.env.retain(|(n, _)| n.as_slice() != name);
        if let Some(value) = value {
            self.description.env.push((name.to_vec(), value.to_vec()));
        }
        true
    }

    /// The superglobal named, `$_SERVER` and its kin.
    #[must_use]
    pub fn superglobal(&self, name: &[u8]) -> Option<&PhpArray> {
        self.superglobals
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, a)| a)
    }

    /// Replace a superglobal, what the GPC seeding and `session_start` do.
    pub fn set_superglobal(&mut self, name: &[u8], value: PhpArray) {
        self.superglobals.retain(|(n, _)| n.as_slice() != name);
        self.superglobals.push((name.to_vec(), value));
    }

    /// `php_build_argv` (`Z/main/php_variables.c:657-713`) under
    /// `register_argc_argv`, which php's CLI forces on: `$argv` is the
    /// host's `argv`, `php_self` first, as a list of strings, and
    /// `$argc` its count, written into `EG(symbol_table)`, which for the
    /// main op array is its CVs of those names (`zend_attach_symbol_table`
    /// at its activation); a request the host describes with no `argv`
    /// at all, php's web hosts, gets neither. `$_SERVER['argv']` and
    /// `$_SERVER['argc']` wait on the superglobal fetch the compiler does
    /// not read yet, and `global $argv` inside a function on the symbol
    /// table (#280).
    ///
    /// ```php
    /// var_dump($argc);
    /// var_dump($argv);
    /// ```
    /// ```expect
    /// int(1)
    /// array(1) {
    ///   [0]=>
    ///   string(%d) "%s"
    /// }
    /// ```
    pub(crate) fn seed_argv(&mut self, frame: FrameRef, unit: &Unit) -> Result<(), Thrown> {
        if self.description.argv.is_empty() {
            return Ok(());
        }
        let argc = i64::try_from(self.description.argv.len()).unwrap_or(i64::MAX);
        let names: Vec<(u32, bool)> = exec::code_of(unit)
            .vars
            .iter()
            .enumerate()
            .filter_map(|(at, name)| match name.0.as_ref() {
                b"argv" => Some((at, true)),
                b"argc" => Some((at, false)),
                _ => None,
            })
            .filter_map(|(at, is_argv)| u32::try_from(at).ok().map(|at| (at, is_argv)))
            .collect();
        for (at, is_argv) in names {
            let value = if is_argv {
                let mut array = PhpArray::new(self)?;
                for word in self.description.argv.clone() {
                    let word = PhpString::new(self, &word)?;
                    array.append(self, Value::string(word))?;
                }
                Value::array(array)
            } else {
                Value::int(argc)
            };
            let old = self.set_slot(frame, at, value);
            self.release(old);
        }
        Ok(())
    }

    /// `php_sapi_name()`: `sapi_module.name`.
    #[must_use]
    pub fn sapi_name(&self) -> &str {
        &self.description.sapi_name
    }

    /// The file the request serves, `$_SERVER['SCRIPT_NAME']`.
    #[must_use]
    pub fn script_name(&self) -> Option<&[u8]> {
        self.description.script_name.as_deref()
    }

    /// The request body as a stream, `php://input` and `STDIN`.
    pub fn stdin(&mut self) -> Stream {
        self.new_stream()
    }

    /// `getcwd()`.
    #[must_use]
    pub fn cwd(&self) -> HostPath {
        HostPath(self.cwd.clone())
    }

    /// `chdir()`.
    pub fn set_cwd(&mut self, path: HostPath) -> Result<(), Thrown> {
        self.cwd = path.0;
        Ok(())
    }

    /// `sys_get_temp_dir()`.
    #[must_use]
    pub fn temp_dir(&self) -> HostPath {
        HostPath(std::env::temp_dir())
    }

    /// `umask()` as it stands.
    #[must_use]
    pub fn umask(&self) -> u32 {
        self.umask
    }

    /// `umask(int)`: the previous mask.
    pub fn set_umask(&mut self, mask: u32) -> u32 {
        std::mem::replace(&mut self.umask, mask)
    }

    /// `strerror` text for an OS error code, as php's `%s` of it.
    #[must_use]
    pub fn os_error_text(&self, code: i32) -> String {
        std::io::Error::from_raw_os_error(code).to_string()
    }

    /// `parse_str()`: a query string as an array, its strings the
    /// request's.
    pub fn parse_query(&mut self, query: &[u8]) -> Result<PhpArray, Thrown> {
        let mut array = PhpArray::new(self)?;
        for pair in query.split(|b| *b == b'&').filter(|p| !p.is_empty()) {
            let mut halves = pair.splitn(2, |b| *b == b'=');
            let name = halves.next().unwrap_or_default();
            let value = halves.next().unwrap_or_default();
            let key = Key::from_string(PhpString::new(self, name)?);
            let value = Value::string(PhpString::new(self, value)?);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// `set_time_limit()`, which is `zend_alter_ini_entry_ex` over
    /// `max_execution_time` at runtime (`Z/main/main.c:1631`): the
    /// entry moved through [`Self::ini_set`], so the timer is re-armed
    /// as it is there, and `true` when the entry took the value.
    pub fn set_time_limit(&mut self, seconds: i64) -> bool {
        self.ini_set(b"max_execution_time", seconds.to_string().as_bytes())
            .is_ok_and(|previous| previous.is_some())
    }

    /// `register_shutdown_function()`, `append_user_shutdown_function`
    /// (`Z/ext/standard/basic_functions.c:1726`): one entry appended at
    /// the end of the list, which is the order
    /// [`crate::natives::call_shutdown_functions`] serves them in. The
    /// callback and the arguments are the entry's owners from here,
    /// given up by [`Request::free_shutdown_functions`].
    pub fn register_shutdown_function(&mut self, callback: Callable, arguments: Vec<Value>) {
        self.shutdown_functions.push((callback, arguments));
    }

    /// The registered entry at `at`, as `zend_hash_apply` reaches it:
    /// the callback and its arguments as views, the list keeping its
    /// owners, so the walk serves what it reads and an entry appended
    /// while it runs is reached by the same walk. `None` past the end
    /// is what ends it.
    pub(crate) fn shutdown_function(&self, at: usize) -> Option<(Callable, Vec<Value>)> {
        self.shutdown_functions.get(at).cloned()
    }

    /// `php_free_shutdown_functions` (`Z/ext/standard/basic_functions.c:1660`),
    /// step 7 of `php_request_shutdown`, behind step 6 and its
    /// `php_output_deactivate`: every entry's callback and argument
    /// released, which is `user_shutdown_function_dtor` over the
    /// destroyed table.
    pub(crate) fn free_shutdown_functions(&mut self) {
        for (callback, arguments) in std::mem::take(&mut self.shutdown_functions) {
            self.release(callback.into_value());
            for value in arguments {
                self.release(value);
            }
        }
    }

    /// `register_tick_function()`.
    pub fn register_tick_function(&mut self, callback: Callable, arguments: Vec<Value>) -> bool {
        self.tick_functions.push((callback, arguments));
        true
    }

    /// `unregister_tick_function()`.
    pub fn unregister_tick_function(&mut self, callback: &Callable) -> bool {
        let before = self.tick_functions.len();
        self.tick_functions.retain(|(c, _)| c != callback);
        self.tick_functions.len() != before
    }

    // -------------------------------------------------- memory and gc

    /// The request's own memory, for the compilation whose unit is this
    /// request's alone (design §5.2): what a host handed the source for,
    /// an `eval` and an uncached include, whose constant strings are
    /// interned here and are gone when the request ends.
    pub(crate) fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// A string of `bytes` out of this request's heap, what
    /// [`PhpString::new`] is: php's `ZVAL_STRINGL_FAST`, the process's
    /// interned string for the empty string and a string of one byte
    /// (`ZSTR_EMPTY_ALLOC`, `ZSTR_CHAR`), `zend_string_init` otherwise.
    /// The memory limit is the fatal that ends the request, recorded
    /// here where `zend_mm` leaves the request, and the `Thrown` that
    /// carries no throwable ends the native.
    pub(crate) fn string(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        let allocated = if bytes.len() <= 1 {
            self.heap.intern(bytes)
        } else {
            self.heap.string(bytes)
        };
        self.allocated(allocated)
    }

    /// A string of `len` bytes written in place, what
    /// [`PhpString::filled`] is: `zend_string_alloc`, then the caller's
    /// write into `ZSTR_VAL`.
    pub(crate) fn string_with(
        &mut self,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<PhpString, Thrown> {
        let allocated = self.heap.string_with(len, fill);
        self.allocated(allocated)
    }

    /// A string of `n * m + l` bytes written in place, what
    /// [`PhpString::product`] is: `zend_string_safe_alloc`.
    pub(crate) fn string_product(
        &mut self,
        n: usize,
        m: usize,
        l: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<PhpString, Thrown> {
        let allocated = self.heap.string_safe_alloc(n, m, l, fill);
        self.allocated(allocated)
    }

    /// What the heap answered, as the native sees it: the string, or the
    /// memory limit as php's fatal, which `zend_mm` raises where the
    /// allocation was asked for and which ends the request from there.
    fn allocated(&mut self, allocated: Result<php_heap::Str, Fatal>) -> Result<PhpString, Thrown> {
        self.fatal_of(allocated).map(PhpString::of)
    }

    /// One more owner of what `value` holds: `Z_TRY_ADDREF_P`, a refcount
    /// for a string, an array, an object, a resource or a reference of
    /// the request's, nothing for a scalar, an interned string or an
    /// immutable array. What a native calls where php's body copies a
    /// value it already holds into a second place (`zend_string_copy`,
    /// `ZVAL_COPY`): the clone of the `Value` is `ZVAL_COPY_VALUE`, and
    /// this beside it is the second owner, given up with whatever row or
    /// property the copy is put in. What a native does before it returns an argument
    /// as its value, `RETURN_COPY` and `RETURN_STR_COPY`: the frame gives
    /// its owner up with the call, and the caller's temporary holds the
    /// one taken here.
    pub fn addref(&self, value: &Value) {
        self.heap.addref(value.to_zval());
    }

    /// One owner fewer of what `value` holds, `zval_ptr_dtor`: the
    /// string, the array, the object, the resource or the reference is
    /// freed when its last owner gives it up, an array's elements and a
    /// reference's value released with it, an object's destructor and a
    /// resource's per-type destructor served first through seam 1, and
    /// an interned string or an immutable array never is. The request
    /// is the hooks over itself, so the heap a destructor allocates from
    /// is this one, handed back for the duration ([`Hooks`]).
    // Taking the value is the point: the owner is given up here, and a
    // `Value` that is still in hand after this call names what may be
    // freed.
    #[allow(clippy::needless_pass_by_value)]
    pub fn release(&mut self, value: Value) {
        Heap::release(&mut Hooks(self), value.to_zval());
    }

    // ------------------------------------------ references, resources

    /// `ZVAL_NEW_REF`, what [`PhpRef::new`] is: a cell of the request's
    /// heap holding `value`, one owner of the cell, `value` the cell's
    /// from here, and the memory limit as the fatal that ends the
    /// request.
    pub(crate) fn reference(&mut self, value: Value) -> Result<PhpRef, Thrown> {
        let allocated = self.heap.reference(value.into_zval());
        self.fatal_of(allocated).map(PhpRef::of)
    }

    /// The reference's half of `zend_assign_to_variable`, what
    /// [`PhpRef::set`] and `ASSIGN` over a CV holding a reference are:
    /// `value` is the cell's from here, and what the cell held is
    /// released last, after the write.
    pub(crate) fn reference_set(&mut self, r: Ref, value: Value) {
        Heap::reference_set(&mut Hooks(self), r, value.into_zval());
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

    /// The type's name as registered.
    pub(crate) fn resource_type_name(&self, kind: ResourceType) -> Option<&[u8]> {
        self.resources.type_name(kind)
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
    /// the object storage is freed.
    pub(crate) fn close_resources(&mut self) {
        Heap::close_resources(&mut Hooks(self));
    }

    // -------------------------------------------------------- arrays

    /// An empty array out of this request's heap, what [`PhpArray::new`]
    /// is: php's `array_init`, and the memory limit as the fatal that
    /// ends the request.
    pub(crate) fn array(&mut self) -> Result<PhpArray, Thrown> {
        let allocated = self.heap.array();
        self.fatal_of(allocated).map(PhpArray::of)
    }

    /// `zend_new_array(size)`, what `INIT_ARRAY` lays down for the
    /// elements it was told to expect, and `zend_hash_real_init_mixed`
    /// over it where the opline says the array cannot be packed
    /// (`ZEND_ARRAY_NOT_PACKED`); the memory limit as the fatal that ends
    /// the request.
    pub(crate) fn array_sized(&mut self, size: u32, packed: bool) -> Result<PhpArray, Thrown> {
        let allocated = if packed {
            self.heap.array_sized(size)
        } else {
            self.heap.array_mixed(size)
        };
        self.fatal_of(allocated).map(PhpArray::of)
    }

    /// `zend_hash_update` or `zend_hash_index_update` over `a`, which
    /// has one owner: `value` is the array's from here, what `key` held
    /// is released, and a string key is one more owner.
    pub(crate) fn array_update(
        &mut self,
        a: Arr,
        key: php_heap::Key,
        value: Value,
    ) -> Result<(), Thrown> {
        let z = value.into_zval();
        let written = Heap::array_update(&mut Hooks(self), a, key, z);
        self.fatal_of(written)
    }

    /// `zend_hash_next_index_insert` over `a`, which has one owner:
    /// `false`, and `value` given up, when the next element is taken.
    pub(crate) fn array_append(&mut self, a: Arr, value: Value) -> Result<bool, Thrown> {
        let z = value.into_zval();
        let appended = self.heap.array_append(a, z);
        if appended == Ok(false) {
            Heap::release(&mut Hooks(self), z);
        }
        self.fatal_of(appended)
    }

    /// `zend_hash_lookup` over `a`, which has one owner: the entry at
    /// `key` in place, laid down holding `null` where the table has none,
    /// which is what a `FETCH_DIM_W` and an `ASSIGN_DIM` write through.
    pub(crate) fn array_lookup(
        &mut self,
        a: Arr,
        key: php_heap::Key,
    ) -> Result<Option<ZvalRef>, Thrown> {
        let entry = self.heap.array_lookup(a, key);
        self.fatal_of(entry)
    }

    /// `zend_hash_next_index_insert` of `null` over `a`, which has one
    /// owner: the entry appended, in place; `None` when the next element
    /// is taken.
    pub(crate) fn array_next_entry(&mut self, a: Arr) -> Result<Option<ZvalRef>, Thrown> {
        let entry = self.heap.array_next_entry(a);
        self.fatal_of(entry)
    }

    /// `zend_hash_del` over `a`, which has one owner: `true` when the key
    /// was there, its value released.
    pub(crate) fn array_del(&mut self, a: Arr, key: php_heap::Key) -> bool {
        Heap::array_del(&mut Hooks(self), a, key)
    }

    /// `zend_array_dup`: a copy of `a` with one owner.
    pub(crate) fn array_dup(&mut self, a: Arr) -> Result<Arr, Thrown> {
        let copied = self.heap.array_dup(a);
        self.fatal_of(copied)
    }

    /// `SEPARATE_ARRAY`: `a` itself with one owner, else a copy of it
    /// with one, `a` one owner fewer.
    pub(crate) fn array_separate(&mut self, a: Arr) -> Result<Arr, Thrown> {
        let separated = self.heap.separate(a);
        self.fatal_of(separated)
    }

    /// `zend_hash_sort` over `a`, which has one owner, keys kept.
    pub(crate) fn array_sort(
        &mut self,
        a: Arr,
        compare: &mut dyn FnMut(Zval, Zval) -> std::cmp::Ordering,
    ) -> Result<(), Thrown> {
        let sorted = self.heap.array_sort(a, compare);
        self.fatal_of(sorted)
    }

    /// What the heap answered, as the native sees it: the value, or the
    /// fatal the allocator raised, recorded here where `zend_mm` leaves
    /// the request, as the `Thrown` carrying no throwable that ends the
    /// native.
    fn fatal_of<T>(&mut self, answered: Result<T, Fatal>) -> Result<T, Thrown> {
        match answered {
            Ok(value) => Ok(value),
            Err(fatal) => {
                self.end_in(&fatal);
                Err(Thrown::of(None))
            }
        }
    }

    /// `zend_set_memory_limit` over the ini entry as the table has it:
    /// at request start, and again when `ini_set()` moves the entry,
    /// which is php's `OnChangeMemoryLimit`. `-1` is no limit.
    fn set_memory_limit(&mut self) {
        let limit = self.ini_parse_bytes_of("memory_limit");
        self.heap
            .set_memory_limit(usize::try_from(limit).unwrap_or(usize::MAX >> 1));
    }

    /// `memory_get_usage()`.
    #[must_use]
    pub fn memory_usage(&self, real: bool) -> usize {
        self.heap.memory_usage(real)
    }

    /// `memory_get_peak_usage()`: the chunks' high-water mark for
    /// `real == true`, the bin accounting's for `real == false`, the two
    /// the heap keeps apart (design §5.13).
    #[must_use]
    pub fn memory_peak(&self, real: bool) -> usize {
        if real {
            self.heap.memory_real_peak()
        } else {
            self.heap.memory_peak()
        }
    }

    /// `memory_reset_peak_usage()`.
    pub fn reset_memory_peak(&mut self) {
        self.heap.reset_peak();
    }

    /// `gc_collect_cycles()`: the number of collected cycles.
    pub fn gc_collect(&mut self) -> i64 {
        0
    }

    /// `gc_enable()` and `gc_disable()`.
    pub fn gc_enable(&mut self, enabled: bool) {
        self.gc_enabled = enabled;
    }

    /// `gc_enabled()`.
    #[must_use]
    pub fn gc_enabled(&self) -> bool {
        self.gc_enabled
    }

    /// `gc_status()` with php 8.5's fields, the keys strings of the
    /// request's.
    pub fn gc_status(&mut self) -> Result<PhpArray, Thrown> {
        let mut array = PhpArray::new(self)?;
        for (name, value) in [
            ("running", Value::bool(false)),
            ("protected", Value::bool(false)),
            ("full", Value::bool(false)),
            ("runs", Value::int(0)),
            ("collected", Value::int(0)),
            ("threshold", Value::int(10_001)),
            ("buffer_size", Value::int(16_384)),
            ("roots", Value::int(0)),
            ("application_time", Value::float(0.0)),
            ("collector_time", Value::float(0.0)),
            ("destructor_time", Value::float(0.0)),
            ("free_time", Value::float(0.0)),
        ] {
            let key = Key::Str(PhpString::new(self, name.as_bytes())?);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    // -------------------------------------------------- paths and streams

    /// A path made absolute against the request's `cwd`.
    #[must_use]
    pub fn absolutize(&self, path: &[u8]) -> HostPath {
        let given = PathBuf::from(String::from_utf8_lossy(path).into_owned());
        if given.is_absolute() {
            HostPath(given)
        } else {
            HostPath(self.cwd.join(given))
        }
    }

    /// `realpath()`: `None` where the path does not exist.
    #[must_use]
    pub fn realpath(&self, path: &[u8]) -> Option<HostPath> {
        std::fs::canonicalize(self.absolutize(path).0)
            .ok()
            .map(HostPath)
    }

    /// The `open_basedir` guard Mahout's `check_fs_access` performed at
    /// 27 sites: php's warning and a `Thrown` when the path is outside.
    pub fn fs_guard(&mut self, path: &HostPath, intent: PathIntent) -> Result<(), Thrown> {
        let _ = (path, intent);
        Ok(())
    }

    /// `is_uploaded_file()`.
    #[must_use]
    pub fn is_uploaded_file(&self, path: &[u8]) -> bool {
        let _ = path;
        false
    }

    /// `move_uploaded_file()`'s first half: take the upload's temporary
    /// path out of the request, once.
    pub fn take_uploaded_file(&mut self, path: &[u8]) -> Option<HostPath> {
        let _ = path;
        None
    }

    fn new_stream(&mut self) -> Stream {
        self.streams = self.streams.saturating_add(1);
        Stream { id: self.streams }
    }

    /// `fopen()` through the wrapper the path's scheme names.
    pub fn open_stream(&mut self, path: &[u8], mode: &[u8]) -> Result<Stream, Thrown> {
        let _ = (path, mode);
        Ok(self.new_stream())
    }

    /// `opendir()`.
    pub fn open_dir(&mut self, path: &[u8]) -> Result<Stream, Thrown> {
        let _ = path;
        Ok(self.new_stream())
    }

    /// `stream_wrapper_register()`: `false` when the scheme is taken.
    pub fn register_wrapper(&mut self, wrapper: StreamWrapper) -> bool {
        if self.wrappers.iter().any(|w| w.scheme == wrapper.scheme) {
            return false;
        }
        self.wrappers.push(wrapper);
        true
    }

    /// `stream_get_wrappers()`.
    #[must_use]
    pub fn stream_wrappers(&self) -> Vec<String> {
        self.wrappers.iter().map(|w| w.scheme.clone()).collect()
    }

    /// `stream_filter_register()`: `false` when the name is taken.
    pub fn register_filter(&mut self, filter: StreamFilter) -> bool {
        if self.filters.iter().any(|f| f.name == filter.name) {
            return false;
        }
        self.filters.push(filter);
        true
    }
}

/// The request's side of the heap's upward seam (design §4.3, seam 1):
/// the request itself, whole, so that `__destruct` is served by the
/// request that owns the frames, and the heap the operation is on is
/// the request's own field, handed back through [`HeapHooks::heap`] at
/// every step. No second `&mut` on the request: the heap never holds a
/// borrow of itself across a call up, which is the discipline php keeps
/// by re-reading `EG(objects_store)` after `dtor_obj`.
struct Hooks<'a>(&'a mut Request);

impl HeapHooks for Hooks<'_> {
    fn heap(&mut self) -> &mut Heap {
        &mut self.0.heap
    }

    /// `dtor_obj`, through the table of the object's class (design §5.4).
    fn destruct(&mut self, obj: Obj) {
        let table = self.0.object_handlers(obj);
        table.dtor_obj(self.0, PhpObject::of(obj));
    }

    /// `free_obj`, through the table: the payload's release is the
    /// class's, and the drop after it is the heap's.
    fn free_native(&mut self, obj: Obj) {
        let table = self.0.object_handlers(obj);
        table.free_obj(self.0, PhpObject::of(obj));
    }

    /// `get_gc`, through the table: what the payload owns, handed to the
    /// collector's walk as zvals.
    fn gc_children(&mut self, obj: Obj, visit: &mut dyn FnMut(Zval)) {
        let table = self.0.object_handlers(obj);
        table.get_gc(self.0, PhpObject::of(obj), &mut |value| {
            visit(value.to_zval());
        });
    }

    /// `list_dtor_ex`: the type's destructor as registered, handed the
    /// resource with the type it had, as php hands it a copy of the
    /// struct; a type with no destructor closes silently.
    fn free_resource(&mut self, res: Res, kind: ResourceType) {
        if let Some(dtor) = self.0.resources.dtor(kind) {
            dtor(self.0, Resource::of(res));
        }
    }

    fn out_of_memory(&mut self, bytes: usize) -> Fatal {
        Fatal::OutOfMemory(bytes)
    }
}

/// What `zend_objects_destroy_object` found in the pending slot before
/// it served a destructor ([`Request::save_pending`]): nothing, one
/// throwable held there for the duration of the call, or the throwable
/// that is the object being destroyed, which is php's one
/// `E_CORE_ERROR` and no call at all.
#[derive(Clone, Copy)]
enum Saved {
    Nothing,
    Throwable(PhpObject),
    TheObject,
}

/// Where `display_errors` sends a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Display {
    Off,
    Body,
    Stderr,
}

/// `PHP_DISPLAY_ERRORS_STDOUT` and `PHP_DISPLAY_ERRORS_STDERR`
/// (`Z/main/php_globals.h:36-37`), the two modes of `PG(display_errors)`
/// beside zero.
const DISPLAY_ERRORS_STDOUT: u8 = 1;
const DISPLAY_ERRORS_STDERR: u8 = 2;

/// `uint8_t mode = ZEND_ATOL(...)` at `Z/main/main.c:513`: the number
/// narrowed to php's byte.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "php's `uint8_t mode = ZEND_ATOL(value)`, whose narrowing is the behaviour being copied"
)]
fn display_errors_mode(number: i64) -> u8 {
    number as u8
}

/// `zend_ini_parse_bool`: an ini entry read as on or off, where "off",
/// "no", "false" and "" are off beside "0" and everything else is on.
fn ini_truth(value: Option<&[u8]>) -> bool {
    let Some(value) = value else { return false };
    let text = value.to_ascii_lowercase();
    !matches!(text.as_slice(), b"" | b"0" | b"off" | b"no" | b"false")
}

/// `ZEND_ATOL`, which is `atoll` (`Z/Zend/zend_long.h:77`): leading
/// whitespace skipped, one sign, the digits up to the first byte that is
/// not one, and zero where there are none; a number past `i64` saturates
/// as `strtoll` does.
fn ini_atol(value: &[u8]) -> i64 {
    let mut bytes = value
        .iter()
        .skip_while(|b| b.is_ascii_whitespace())
        .peekable();
    let negative = match bytes.peek() {
        Some(b'-') => {
            bytes.next();
            true
        }
        Some(b'+') => {
            bytes.next();
            false
        }
        _ => false,
    };
    let mut n: i64 = 0;
    for digit in bytes.take_while(|b| b.is_ascii_digit()) {
        let digit = i64::from(digit - b'0');
        n = if negative {
            n.checked_mul(10)
                .and_then(|n| n.checked_sub(digit))
                .unwrap_or(i64::MIN)
        } else {
            n.checked_mul(10)
                .and_then(|n| n.checked_add(digit))
                .unwrap_or(i64::MAX)
        };
    }
    n
}

/// `zend_visibility_string` (`Z/Zend/zend_compile.c:1063`): the word
/// php's bad-call texts name a method's visibility by.
fn visibility_string(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Public => "public",
        Visibility::Protected => "protected",
        Visibility::Private => "private",
    }
}

/// One string property of a throwable, empty where there is none.
fn string_prop(req: &Request, object: PhpObject, name: &[u8]) -> String {
    object
        .prop(req, name)
        .and_then(|value| {
            value
                .as_str_bytes()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        })
        .unwrap_or_default()
}

/// php's `error_type_str` (`Z/main/main.c:1414-1448`), the word both
/// renderings name the level by. `E_RECOVERABLE_ERROR` is the one level
/// whose word is not its family's, and `E_STRICT` is the one level php
/// 8.5 has no case for, so it falls to the `default:` every level php
/// dropped falls to.
///
/// No fence reaches either word, so the two arms are held by the test
/// below and not by a witness. php 8.5 raises `E_RECOVERABLE_ERROR` at
/// one site, `zend_object_is_true` (`Z/Zend/zend_operators.c:2918`),
/// when an object's `cast_object` handler refuses `_IS_BOOL`; the
/// standard handler answers `true` for every user class, and no class
/// in the bundle brings a handler that refuses, so no PHP source raises
/// it, and `trigger_error()` takes the four `E_USER_*` levels alone.
/// `E_STRICT` has no raise anywhere: its constant stands `#[Deprecated]`
/// since 8.4 with the message that the level was removed.
/// `E_FATAL_ERRORS` (`Z/Zend/zend_errors.h:47`): the six levels
/// `php_error_cb` bails out of, and the ones `zend_error_impl` fetches a
/// trace for under `fatal_error_backtraces`.
fn fatal_errors(level: Level) -> bool {
    matches!(
        level,
        Level::Error
            | Level::CoreError
            | Level::CompileError
            | Level::UserError
            | Level::RecoverableError
            | Level::Parse
    )
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Error | Level::CoreError | Level::CompileError | Level::UserError => "Fatal error",
        Level::RecoverableError => "Recoverable fatal error",
        Level::Warning | Level::CoreWarning | Level::CompileWarning | Level::UserWarning => {
            "Warning"
        }
        Level::Parse => "Parse error",
        Level::Notice | Level::UserNotice => "Notice",
        Level::Deprecated | Level::UserDeprecated => "Deprecated",
        Level::Strict => "Unknown error",
    }
}

/// php's `escape_html` (`Z/main/main.c:1038-1050`): the message of an
/// `E_ERROR` or an `E_PARSE` through `php_escape_html_entities_ex` at
/// `ENT_COMPAT` with `double_encode`, which is `htmlspecialchars`'s four
/// — `&`, `<`, `>` and `"`, an already-written entity escaped again —
/// and leaves `'` alone. php's retry with the invalid bytes substituted
/// is for a message that is not its charset, and a message this engine
/// renders is a `&str`.
fn escape_html(message: &str) -> String {
    let mut escaped = String::with_capacity(message.len());
    for letter in message.chars() {
        match letter {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(letter),
        }
    }
    escaped
}

/// `smart_str_append_escaped` (`Z/Zend/zend_smart_str.c:77`): a byte
/// under 32, over 126 or a backslash written as `\n`, `\r`, `\t`, `\f`,
/// `\v`, `\\`, `\e` or `\xNN` with the digits in upper case, and every
/// other byte as itself. What a string argument in a trace and
/// `zend_print_zval_r`'s cousins write.
pub(crate) fn append_escaped(out: &mut String, bytes: &[u8]) {
    for &byte in bytes {
        match byte {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0c => out.push_str("\\f"),
            0x0b => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            0x1b => out.push_str("\\e"),
            byte if !(32..=126).contains(&byte) => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push_str("\\x");
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0xf)]));
            }
            byte => out.push(char::from(byte)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Level, Request, append_escaped, escape_html, level_name};
    use crate::engine::{Engine, RequestDescription};

    /// A class declaring `__destruct`, as an extension declares one:
    /// what [`Request::destruct`] looks for on the class of the object
    /// it is handed. Its entry is never served, since the branch under
    /// test refuses the call before it.
    fn destructible() -> php_unit::decl::ClassDecl {
        use php_unit::decl::{
            ClassDecl, ClassFlags, ClassKind, EnumBacking, FunctionDecl, MethodDecl, Name,
            Visibility,
        };
        ClassDecl {
            name: Name(b"Destructible".as_slice().into()),
            kind: ClassKind::Class,
            flags: ClassFlags::default(),
            parent: None,
            interfaces: Vec::new(),
            traits: Vec::new(),
            backing: EnumBacking::Pure,
            constants: Vec::new(),
            properties: Vec::new(),
            methods: vec![MethodDecl {
                function: FunctionDecl {
                    name: Name(b"__destruct".as_slice().into()),
                    parameters: Vec::new(),
                    return_type: None,
                    returns_by_ref: false,
                    attributes: Vec::new(),
                    doc_comment: None,
                    file: Name(Box::new([])),
                    line: 0,
                    end_line: 0,
                    frameless: Vec::new(),
                },
                visibility: Visibility::Public,
                is_instance: true,
                is_abstract: false,
                is_final: false,
                tentative_return: false,
            }],
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
        }
    }

    /// The entry of the `__destruct` above: the branch under test
    /// refuses the call, so a raise here is the test failing.
    fn never_served(req: &mut Request) -> Result<crate::api::values::Value, super::Thrown> {
        Err(crate::api::raising::throw(
            req,
            "Error",
            "the destructor was served",
        ))
    }

    /// The same entry, serving: what a native class's `__destruct` is,
    /// a Rust body on the request that served it.
    fn writes_d(req: &mut Request) -> Result<crate::api::values::Value, super::Thrown> {
        let text = crate::api::values::PhpString::new(req, b"d\n")?;
        req.write(text.as_bytes());
        Ok(crate::api::values::Value::null())
    }

    /// Design §5.10 and seam 1: a native class's `__destruct` is served
    /// at the last release like a user class's, through the same
    /// `Request::destruct` and the native branch of `serve_call`. No
    /// witness can hold this one, because the class an extension of
    /// this engine declares is no class of the oracle's, so the
    /// `ASSIGN` the witness beside `handlers::assign` pins is served
    /// here over a class whose body is Rust.
    #[test]
    fn a_native_classs_destructor_is_served_at_the_last_release() {
        let decl = destructible();
        let Some(method) = decl.methods.first().cloned() else {
            return;
        };
        let destructible = crate::api::abi::ExtensionBuilder::default()
            .classes(vec![(
                decl,
                vec![(method, writes_d as crate::api::abi::Entry)],
            )])
            .build("destructible");
        let engine = Engine::builder().extensions(&[destructible]).build();

        let served = engine.serve_capture(
            "<?php $a = new Destructible(); $a = 1; echo \"after\\n\";",
            crate::test::Capture::default(),
        );

        assert_eq!(String::from_utf8_lossy(&served.stdout), "d\nafter\n");
        assert_eq!(served.fatal, None);
    }

    /// `zend_objects_destroy_object`'s `E_CORE_ERROR`
    /// (`Z/Zend/zend_objects.c:157`): a destructor served on the
    /// throwable that is itself pending ends the request there, with no
    /// site, and the destructor is not served. No PHP snippet reaches
    /// it, here or in php, whose corpus names it nowhere, and
    /// [`Request::save_pending`] says which of php's releases would
    /// have to be the one that reaches it. So the state is built here
    /// instead of witnessed, which is what harness §8 allows and #390
    /// settled. The fatal carries no trace of its own, because php
    /// fetches one only while nothing is pending, and here the
    /// throwable is.
    #[test]
    fn a_destructor_served_on_the_pending_throwable_is_the_core_fatal() {
        let decl = destructible();
        let Some(method) = decl.methods.first().cloned() else {
            return;
        };
        let destructible = crate::api::abi::ExtensionBuilder::default()
            .classes(vec![(
                decl,
                vec![(method, never_served as crate::api::abi::Entry)],
            )])
            .build("destructible");
        let engine = Engine::builder().extensions(&[destructible]).build();
        let mut request = request(&engine);
        let Ok(object) = request.new_object_of("Destructible") else {
            return;
        };
        request.pending = Some(object);

        request.destruct(object);

        assert!(
            request.pending.is_some(),
            "the throwable is pending still, since nothing was served"
        );
        assert_eq!(
            String::from_utf8_lossy(&request.take_body()),
            "\nFatal error: Attempt to destruct pending exception in Unknown on line 0\n"
        );
    }

    /// A request on a fresh engine's shared state, for a test that gives
    /// `note_error_in` a level no raiser reaches yet.
    fn request(engine: &Engine) -> Request {
        Request::new(
            engine.shared.clone(),
            RequestDescription {
                sapi_name: String::from("cli"),
                ..RequestDescription::default()
            },
        )
    }

    /// `(EG(error_reporting) & type) || (type & E_CORE)`: a core level
    /// is rendered under `error_reporting=0`, in the same word its
    /// family takes, and every other level is masked. The one raiser
    /// that reaches `E_CORE_ERROR` is
    /// [`Request::save_pending`], which no PHP snippet reaches, and no
    /// raiser reaches `E_CORE_WARNING`, so the mask is held here and not
    /// by a witness. `E_CORE_ERROR` is in
    /// `E_FATAL_ERRORS`, so under php's default `fatal_error_backtraces`
    /// it fetches its trace, `#0 {main}` with no frame standing, and the
    /// warning fetches none.
    #[test]
    fn a_core_level_is_rendered_past_error_reporting() {
        let engine = Engine::builder().ini("error_reporting", "0").build();
        let file = b"witness.php";
        let mut request = request(&engine);
        request.note_error_in(Level::CoreWarning, "core", file, 2);
        request.note_error_in(Level::CoreError, "core", file, 3);
        request.note_error_in(Level::Warning, "masked", file, 4);
        request.note_error_in(Level::Error, "masked", file, 5);
        assert_eq!(
            String::from_utf8_lossy(&request.take_body()),
            "\nWarning: core in witness.php on line 2\n\nFatal error: core in witness.php on line 3\nStack trace:\n#0 {main}\n"
        );
    }

    /// php's second html format (`Z/main/main.c:1475`), the one every
    /// fatal level but `E_ERROR` and `E_PARSE` takes: the message as it
    /// is, and the trace between the `<br />` and the newline. What the
    /// pinned oracle writes for `-d html_errors=1` over
    /// `<?php\nini_set('fatal_error_backtraces', '1'); trigger_error('b<o>om', E_USER_ERROR);`
    /// is the 8.4 deprecation and then
    /// `<br />\n<b>Fatal error</b>:  b<o>om in <b>%s</b> on line <b>2</b><br />\nStack trace:\n#0 %s(2): trigger_error('b<o>om', 256)\n#1 {main}\n`;
    /// no native here raises `E_USER_ERROR`, so the request has no frame
    /// and the trace is `{main}` alone. The same level under
    /// `fatal_error_backtraces=0` fetches none, and the two formats then
    /// write the same bytes but for the escaping.
    #[test]
    fn the_second_html_format_carries_the_trace_after_the_line() {
        let file = b"witness.php";
        let engine = Engine::builder().ini("html_errors", "1").build();
        let mut req = request(&engine);
        req.note_error_in(Level::UserError, "b<o>om", file, 2);
        req.note_error_in(Level::Error, "b<o>om", file, 3);
        assert_eq!(
            String::from_utf8_lossy(&req.take_body()),
            "<br />\n<b>Fatal error</b>:  b<o>om in <b>witness.php</b> on line <b>2</b><br />\nStack trace:\n#0 {main}\n\
             <br />\n<b>Fatal error</b>:  b&lt;o&gt;om in <b>witness.php</b> on line <b>3</b><br />\n"
        );

        let engine = Engine::builder()
            .ini("html_errors", "1")
            .ini("fatal_error_backtraces", "0")
            .build();
        let mut req = request(&engine);
        req.note_error_in(Level::UserError, "b<o>om", file, 2);
        assert_eq!(
            String::from_utf8_lossy(&req.take_body()),
            "<br />\n<b>Fatal error</b>:  b<o>om in <b>witness.php</b> on line <b>2</b><br />\n"
        );
    }

    /// The log form and the xml fault carry the trace after the site as
    /// the plain form does, `ZSTR_LEN(backtrace) ? "\nStack trace:\n" : ""`
    /// in each of `php_error_cb`'s `spprintf` calls. What the pinned
    /// oracle writes for `-d log_errors=1 -d display_errors=0` over
    /// `<?php\n(unset)$x;` is
    /// `PHP Fatal error:  The (unset) cast is no longer supported in %s on line 2\nStack trace:\n#0 {main}`,
    /// and for `-d xmlrpc_errors=1` the fault's string is
    /// `Fatal error:The (unset) cast … on line 2\nStack trace:\n#0 {main}`.
    #[test]
    fn the_log_form_and_the_xml_fault_carry_the_trace() {
        let file = b"witness.php";
        let engine = Engine::builder()
            .ini("log_errors", "1")
            .ini("xmlrpc_errors", "1")
            .build();
        let mut req = request(&engine);
        req.note_error_in(Level::CompileError, "refused", file, 2);
        assert_eq!(
            req.take_aside(),
            vec![super::Aside::Log(
                b"PHP Fatal error:  refused in witness.php on line 2\nStack trace:\n#0 {main}"
                    .to_vec()
            )]
        );
        assert_eq!(
            String::from_utf8_lossy(&req.take_body()),
            "<?xml version=\"1.0\"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>0</int></value></member><member><name>faultString</name><value><string>Fatal error:refused in witness.php on line 2\nStack trace:\n#0 {main}</string></value></member></struct></value></fault></methodResponse>"
        );
    }

    /// `smart_str_append_escaped`'s table, one byte per arm: the seven
    /// named escapes, the hex form for the rest of the control range and
    /// everything past ASCII, and a printable byte untouched.
    #[test]
    fn a_trace_escapes_a_string_as_smart_str_does() {
        let mut out = String::new();
        append_escaped(&mut out, b"a\n\r\t\x0c\x0b\\\x1b\x01\x7f\xff'z");
        assert_eq!(out, "a\\n\\r\\t\\f\\v\\\\\\e\\x01\\x7F\\xFF'z");
    }

    /// php's `error_type_str` for every level `Level` names, the switch
    /// at `Z/main/main.c:1414-1448` read case by case. No PHP source
    /// raises `E_RECOVERABLE_ERROR` or `E_STRICT` under php 8.5, as the
    /// rustdoc beside `level_name` says, so this is the one place the
    /// two words are held.
    #[test]
    fn every_level_is_named_as_php_names_it() {
        for (level, name) in [
            (Level::Error, "Fatal error"),
            (Level::CoreError, "Fatal error"),
            (Level::CompileError, "Fatal error"),
            (Level::UserError, "Fatal error"),
            (Level::RecoverableError, "Recoverable fatal error"),
            (Level::Warning, "Warning"),
            (Level::CoreWarning, "Warning"),
            (Level::CompileWarning, "Warning"),
            (Level::UserWarning, "Warning"),
            (Level::Parse, "Parse error"),
            (Level::Notice, "Notice"),
            (Level::UserNotice, "Notice"),
            (Level::Deprecated, "Deprecated"),
            (Level::UserDeprecated, "Deprecated"),
            (Level::Strict, "Unknown error"),
        ] {
            assert_eq!(level_name(level), name, "{level:?}");
        }
    }

    /// What php's `escape_html` writes, from the pinned oracle under the
    /// witness recipe with `-d html_errors=1`: `<?php\necho <;` is a
    /// parse error whose message names the token it did not want, and
    /// php writes `syntax error, unexpected token &quot;&lt;&quot;`.
    /// `htmlspecialchars($s, ENT_COMPAT)` over `<a&b'c"d` under the same
    /// oracle is `&lt;a&amp;b'c&quot;d`, which is the whole set: the
    /// apostrophe `ENT_QUOTES` would take stands as it is.
    #[test]
    fn the_message_of_a_fatal_is_escaped_as_php_escapes_it() {
        assert_eq!(
            escape_html("syntax error, unexpected token \"<\""),
            "syntax error, unexpected token &quot;&lt;&quot;"
        );
        assert_eq!(escape_html("<a&b'c\"d"), "&lt;a&amp;b'c&quot;d");
        assert_eq!(
            escape_html("&amp; &gt;"),
            "&amp;amp; &amp;gt;",
            "php passes double_encode, so a written entity is written again"
        );
        assert_eq!(escape_html("nothing to escape"), "nothing to escape");
    }
}
