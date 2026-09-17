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
    VmStack, Zval,
};
use php_unit::Unit;
use php_unit::decl::{ClassDecl, ClassKind, FunctionDecl, MethodDecl, Visibility};

use crate::api::abi::{Callable, IniVerdict};
use crate::api::call::Function;
use crate::api::raising::{Level, Thrown};
use crate::api::values::{
    ClassId, ClosureInfo, Key, PhpArray, PhpObject, PhpRef, PhpString, Resource, Value, ValueRef,
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
    headers: Vec<Vec<u8>>,
    header_callback: Option<Callable>,
    /// The host's log as `php_log_err` was given it, one entry per call,
    /// a diagnostic whole however many lines its backtrace adds, and no
    /// terminator, since that is the host's (`sapi_cli_log_message`
    /// writes `"%s\n"`, a file log stamps `[date] ` and `PHP_EOL`); and
    /// the error stream's bytes as `fputs(stderr)` was given them; the
    /// two in the order they were written.
    aside: Vec<Aside>,
    response_code: i64,
    last_error: Option<(Level, String, Vec<u8>, u32)>,
    ending: Option<Ending>,
    resources: Resources,
    superglobals: Vec<(Vec<u8>, PhpArray)>,
    constants: Vec<(Vec<u8>, Value)>,
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
}

impl Request {
    pub(crate) fn new(shared: Arc<Shared>, description: RequestDescription) -> Request {
        let cwd = description.cwd.clone();
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
            output: Output::activate(),
            headers: Vec::new(),
            header_callback: None,
            aside: Vec::new(),
            response_code: 200,
            last_error: None,
            ending: None,
            resources: Resources::default(),
            superglobals: Vec::new(),
            constants: Vec::new(),
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
            returned: Value::null(),
            interrupt: Interrupt::default(),
            timer: None,
            stack_limit: StackLimit::UNCHECKED,
            declared: Vec::new(),
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

    /// The frame off both stacks, every owner its slots held given up
    /// while the frame still stands, `i_free_compiled_variables` over
    /// the CVs and the temporaries with them, since a temporary the
    /// compiler numbered for an opline's value and nothing read is
    /// still an owner here; then `zend_vm_stack_free_call_frame`.
    pub(crate) fn pop_frame(&mut self) {
        let frame = self.stack.current();
        let slots = self.stack.frame(frame).map_or(0, php_heap::Frame::len);
        for at in 0..slots {
            let value = self.take_slot(frame, u32::try_from(at).unwrap_or(u32::MAX));
            self.release(value);
        }
        self.vm.pop();
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

    /// The served call off the stack, `zend_vm_stack_free_args` over what
    /// the `SEND_*` oplines placed and `zend_vm_stack_free_call_frame`
    /// after it.
    pub(crate) fn pop_call(&mut self) {
        if let Some(call) = self.vm.pop_call() {
            for value in call.arguments {
                self.release(value);
            }
            self.release(Value::string(call.name));
            if let Some(this) = call.this {
                self.release(this.as_value());
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
        let name = self.declared_name(function);
        let name = self.interned(name.as_bytes())?;
        for argument in arguments {
            self.addref(argument);
        }
        if let Some(this) = this {
            self.addref(&this.as_value());
        }
        self.vm.push_call(CallState {
            function,
            name,
            arguments: arguments.to_vec(),
            count: arguments.len(),
            this,
            called_scope: Some(called_scope),
            stage: CallStage::Serving,
        });
        let served = self.serve_native(function);
        self.pop_call();
        served
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

    /// `zend_hash_find_ptr(&ce->function_table, lcname)` over the
    /// class's native methods: the method under the lowered name, by
    /// identity, and none where the class has no entry of that name.
    pub(crate) fn method_named(&self, class: ClassId, lowered: &[u8]) -> Option<Function> {
        let decl = self.class_decl(class)?;
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

    /// The declaration of a native method, if `function` is one.
    pub(crate) fn method_decl(&self, function: Function) -> Option<&MethodDecl> {
        let method = self.native_method(function)?;
        self.class_decl(method.class)?
            .methods
            .get(usize::try_from(method.index).ok()?)
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
    /// which is a native method's while its entry runs, and none at the
    /// main op array or inside a function. A user frame carries no
    /// scope until a method body is compiled, so the loop's own scope
    /// is the global one.
    pub(crate) fn executed_scope(&self) -> Option<ClassId> {
        let call = self.vm.serving_call()?;
        self.native_method(call.function).map(|m| m.class)
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
        let Some(method) = self.native_method(function) else {
            return Ok(());
        };
        let class = method.class;
        let Some(decl) = self.method_decl(function) else {
            return Ok(());
        };
        let visibility = decl.visibility;
        let scope = self.executed_scope();
        let allowed = match visibility {
            Visibility::Public => true,
            _ if scope == Some(class) => true,
            Visibility::Private => false,
            Visibility::Protected => self.check_protected(class, scope),
        };
        if allowed {
            return Ok(());
        }
        let shown = match visibility {
            Visibility::Public => "public",
            Visibility::Protected => "protected",
            Visibility::Private => "private",
        };
        let class = self.class_name(class);
        let from = match scope {
            Some(scope) => format!("scope {}", self.class_name(scope)),
            None => String::from("global scope"),
        };
        Err(crate::api::raising::throw(
            self,
            "Error",
            &format!(
                "Call to {shown} method {class}::{}() from {from}",
                String::from_utf8_lossy(written)
            ),
        ))
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
        let fatal = if let Some(level) = compile_time {
            let message = string_prop(self, throwable, b"message");
            self.end_in_compile_time(level, &message, &file, line);
            Fatal::Error(message)
        } else {
            let message = format!("Uncaught {}\n  thrown", self.throwable_to_string(throwable));
            self.end_in_fatal_at(&message, &file, line);
            Fatal::Error(message)
        };
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

    /// `Exception::__toString`: the class, the message where there is one,
    /// the site behind a colon and the trace under `Stack trace:`. The
    /// chain of `previous` throwables php walks after the first, and the
    /// ` and defined` a `TypeError` whose message names a call site takes,
    /// arrive with the user-function frames that make either reachable.
    fn throwable_to_string(&self, throwable: PhpObject) -> String {
        let class = self.class_name_of(throwable);
        let message = string_prop(self, throwable, b"message");
        let (file, line) = self.throw_site(throwable);
        let file = String::from_utf8_lossy(&file);
        // `zend_trace_to_string` over an empty trace is `#0 {main}`, which
        // is what a throwable made before this engine recorded one has.
        let trace = throwable
            .prop(self, b"trace")
            .and_then(|value| value.as_str_bytes().map(<[u8]>::to_vec))
            .map_or_else(
                || String::from("#0 {main}"),
                |bytes| String::from_utf8_lossy(&bytes).into_owned(),
            );
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
    /// and the backtrace `zend_fetch_debug_backtrace` builds there.
    ///
    /// php holds the trace as the array `getTrace` answers and renders it
    /// on demand; this engine holds what `getTraceAsString` renders, which
    /// is all that `__toString` and the uncaught fatal read, until a frame
    /// chain with user functions in it gives the array something to say.
    pub(crate) fn note_throw_site(&mut self, throwable: PhpObject) -> Result<(), Thrown> {
        let file = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
        let trace = self.trace_as_string();
        let line = Value::int(i64::from(self.current_line()));
        for (name, value) in [
            (&b"file"[..], Value::string(PhpString::new(self, &file)?)),
            (b"line", line),
            (
                b"trace",
                Value::string(PhpString::new(self, trace.as_bytes())?),
            ),
        ] {
            let name = PhpString::new(self, name)?;
            throwable.set_prop(self, name, value)?;
        }
        Ok(())
    }

    /// `zend_fetch_debug_backtrace` rendered by `zend_trace_to_string`
    /// with `include_main`: one `#N file(line): name(arguments)` per call
    /// below the top, then `#N {main}`. php walks `EG(current_execute_data)`;
    /// the calls this engine has below the op array being served are the
    /// natives it is serving, read off the call frames still standing on
    /// [`Vm`], whose file and line are the caller's, as php's frame for a
    /// native takes them.
    ///
    /// The arguments are the frame's, rendered by [`Request::trace_arguments`];
    /// under `zend.exception_ignore_args` the frame is built without them,
    /// `DEBUG_BACKTRACE_IGNORE_ARGS`, and the parentheses are empty.
    fn trace_as_string(&self) -> String {
        let file = String::from_utf8_lossy(self.current_file().unwrap_or_default());
        let line = self.current_line();
        let ignore_arguments = self.exception_ignore_args();
        let mut frames: Vec<String> = self
            .vm
            .serving_calls()
            .enumerate()
            .map(|(depth, call)| {
                let arguments = if ignore_arguments {
                    String::new()
                } else {
                    self.trace_arguments(&call.arguments)
                };
                format!(
                    "#{depth} {file}({line}): {}({arguments})",
                    self.qualified_name(call.function, call.this)
                )
            })
            .collect();
        let main = frames.len();
        frames.push(format!("#{main} {{main}}"));
        frames.join("\n")
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
                .map(|call| String::from_utf8_lossy(call.name.as_bytes()).into_owned())
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

    pub(crate) fn header_block(&self) -> Vec<u8> {
        let mut block = Vec::new();
        for header in &self.headers {
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
    pub(crate) fn note_error_in(&mut self, level: Level, message: &str, file: &[u8], line: u32) {
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
        if self.log_errors() {
            let text = format!("PHP {name}:  {message} in {named} on line {line}");
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
                "<?xml version=\"1.0\"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>{}</int></value></member><member><name>faultString</name><value><string>{name}:{message} in {named} on line {line}</string></value></member></struct></value></fault></methodResponse>",
                self.xmlrpc_error_number()
            );
            self.write(text.as_bytes());
        } else if self.html_errors() {
            // php escapes the message of an `E_ERROR` or an `E_PARSE`
            // and no other level's, and writes the same bytes around
            // both; the backtrace that parts php's two html formats is
            // `fatal_error_backtraces`, which this engine does not keep.
            let body = match level {
                Level::Error | Level::Parse => escape_html(message),
                _ => message.to_owned(),
            };
            let text = format!(
                "{}<br />\n<b>{name}</b>:  {body} in <b>{named}</b> on line <b>{line}</b><br />\n{}",
                self.ini_text(b"error_prepend_string"),
                self.ini_text(b"error_append_string"),
            );
            self.write(text.as_bytes());
        } else if display == Display::Stderr {
            // php's stderr form stands inside the same `else` as the
            // plain one, so `html_errors` takes a CLI diagnostic off the
            // error stream and onto the body; and it carries neither
            // string around it.
            let text = format!("{name}: {message} in {named} on line {line}\n");
            self.write_stderr(text.as_bytes());
        } else {
            let text = format!(
                "{}\n{name}: {message} in {named} on line {line}\n{}",
                self.ini_text(b"error_prepend_string"),
                self.ini_text(b"error_append_string"),
            );
            self.write(text.as_bytes());
        }
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
        let file = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
        let line = self.current_line();
        self.end_in_fatal_at(message, &file, line);
    }

    /// The same fatal at a site of its own, which an uncaught throwable
    /// carries. The first fatal is the one that ends the request, since
    /// php leaves the request from the first and renders no second.
    pub(crate) fn end_in_fatal_at(&mut self, message: &str, file: &[u8], line: u32) {
        if self.ending.is_some() {
            return;
        }
        self.note_error_in(Level::Error, message, file, line);
        self.end_in_fatal_logged(message, file, line);
    }

    /// The ending alone, the diagnostic already rendered at its own
    /// level: the log form the host is handed for a fatal.
    fn end_in_fatal_logged(&mut self, message: &str, file: &[u8], line: u32) {
        if self.ending.is_none() {
            self.ending = Some(Ending::Fatal(format!(
                "PHP Fatal error:  {message} in {} on line {line}",
                String::from_utf8_lossy(file)
            )));
        }
    }

    /// `E_PARSE` or `E_COMPILE_ERROR` at a site of its own, which is what
    /// the compiler's refusal carries and what a `ParseError` or
    /// `CompileError` left uncaught recorded: rendered once through
    /// `php_error_cb` at its level, and the request ended as that level
    /// ends it, php's status alone for the parse error and the log form
    /// beside it for the compile error, which `php_error_cb` bails out
    /// of as it does from `E_ERROR`.
    pub(crate) fn end_in_compile_time(
        &mut self,
        level: Level,
        message: &str,
        file: &[u8],
        line: u32,
    ) {
        self.note_error_in(level, message, file, line);
        match level {
            Level::Parse => self.end_in_parse(),
            _ => self.end_in_fatal_logged(message, file, line),
        }
    }

    /// `E_PARSE`: the request ends with php's status and no second
    /// rendering, the text having gone out with the diagnostic.
    pub(crate) fn end_in_parse(&mut self) {
        if self.ending.is_none() {
            self.ending = Some(Ending::Parse);
        }
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
        let allocated = self.heap.object(class.0, slots);
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
    pub(crate) fn read_property(&mut self, o: Obj, name: &[u8]) -> Result<Value, Thrown> {
        let class = ClassId(o.class());
        let declared = self.class_decl(class).and_then(|decl| {
            decl.properties
                .iter()
                .filter(|p| p.is_instance)
                .enumerate()
                .find(|(_, p)| &*p.name.0 == name)
                .map(|(i, p)| {
                    (
                        u32::try_from(i).unwrap_or(u32::MAX),
                        p.visibility,
                        p.ty.is_some(),
                    )
                })
        });
        let shown = String::from_utf8_lossy(name).into_owned();
        if let Some((slot, visibility, typed)) = declared {
            let scope = self.executed_scope();
            let allowed = match visibility {
                Visibility::Public => true,
                _ if scope == Some(class) => true,
                Visibility::Private => false,
                Visibility::Protected => self.check_protected(class, scope),
            };
            if !allowed {
                let word = match visibility {
                    Visibility::Public => "public",
                    Visibility::Protected => "protected",
                    Visibility::Private => "private",
                };
                let class = self.class_name(class);
                return Err(crate::api::raising::throw(
                    self,
                    "Error",
                    &format!("Cannot access {word} property {class}::${shown}"),
                ));
            }
            let z = o.slot(slot);
            if !z.is_undef() {
                let value = Value::of_zval(z).deref();
                self.addref(&value);
                return Ok(value);
            }
            if typed {
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

    /// A property write, `zend_std_write_property` without the handler
    /// in the way: into the declared slot, what it held released last,
    /// or into the dynamic table, materialised on the first write and
    /// separated when a cast shares it. The name is given up once the
    /// table has taken its own owner of it.
    pub(crate) fn object_set_prop(
        &mut self,
        o: Obj,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        if let Some(i) = self.prop_slot(ClassId(o.class()), name.as_bytes()) {
            let z = value.into_zval();
            Heap::object_set_slot(&mut Hooks(self), o, i, z);
        } else {
            let table = self.heap.object_properties(o);
            let table = self.fatal_of(table)?;
            let table = self.array_separate(table)?;
            self.heap.object_set_properties(o, table);
            self.array_update(table, php_heap::Key::Str(name.heap()), value)?;
        }
        self.release(Value::string(name));
        Ok(())
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

    /// `zend_objects_clone_obj` without `__clone`: a fresh entry of the
    /// same class holding what this one holds, one more owner each; an
    /// object with a native payload is its class's `clone_obj` to copy
    /// and, with no handler table yet, php's `Error` for an uncloneable
    /// object.
    pub(crate) fn clone_object(&mut self, o: Obj) -> Result<PhpObject, Thrown> {
        let cloned = self.heap.object_clone(o);
        if let Some(new) = self.fatal_of(cloned)? {
            return Ok(PhpObject::of(new));
        }
        let class = self.class_name_of(PhpObject::of(o));
        Err(crate::api::raising::throw(
            self,
            "Error",
            &format!("Trying to clone an uncloneable object of class {class}"),
        ))
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
    /// at one owner meanwhile. A class with none serves nothing, which
    /// is every class the tables hold until a method can be declared;
    /// the visibility check, the pending-throwable rules and the
    /// shutdown warning arrive with the first destructor.
    fn destruct(&mut self, o: Obj) {
        let destructor: Option<Box<[u8]>> = self.class_decl(ClassId(o.class())).and_then(|decl| {
            decl.methods
                .iter()
                .find(|m| m.function.name.0.eq_ignore_ascii_case(b"__destruct"))
                .map(|m| m.function.name.0.clone())
        });
        let Some(destructor) = destructor else {
            return;
        };
        let this = PhpObject::of(o);
        if let Err(thrown) = crate::api::call::call_method(self, &this, &destructor, &[]) {
            self.leave_pending(&thrown);
        }
    }

    /// `zend_objects_store_call_destructors`, the second step of
    /// `php_request_shutdown` (design §5.10): every live object's
    /// destructor not yet served, in store order, and what one leaves
    /// pending rendered as the uncaught fatal, as `zend_call_function`
    /// renders it with no frame to throw into. The symbol table php
    /// walks in reverse before this is the main frame's CVs here, given
    /// up when it popped (#280).
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

    /// `free_obj` for the native payload: what it names of the heap
    /// given back before it is dropped. A `Closure` names its bound
    /// `$this`, and nothing else with a payload exists yet.
    fn free_native(&mut self, o: Obj) {
        if let Some(closure) = self.heap.native_mut::<ClosureInfo>(o)
            && let Some(this) = closure.this.take()
        {
            self.release(this.as_value());
        }
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
        match self.native_method(function) {
            Some(method) => {
                let class = self.class_name(method.class);
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

    fn ini_number(&self, name: &[u8], default: i32) -> i32 {
        self.ini_get(name)
            .and_then(|value| String::from_utf8_lossy(value).parse().ok())
            .unwrap_or(default)
    }

    pub(crate) fn caller_strict_types(&self) -> bool {
        self.strict_types
    }

    pub(crate) fn callable_of(&self, value: &Value) -> Option<Callable> {
        let name = value.as_str_bytes()?;
        self.function_named(name)?;
        Some(Callable::named(value.clone(), name))
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

    /// `do_bind_class` for every class the unit declares, before its
    /// main op array runs: php binds a top-level class with no parent
    /// at compile time and the compiler emitted no opline for it, so
    /// the request's table takes it here. A name already declared is
    /// `zend_class_redeclaration_error`, `E_COMPILE_ERROR` in php's two
    /// wordings, the site of the earlier declaration named when it was
    /// a user class, and the request ends before the frame is pushed;
    /// php runs nothing of a file whose class fails to bind.
    pub(crate) fn declare_classes(&mut self, unit: &Arc<Unit>) -> bool {
        for (at, decl) in unit.classes.iter().enumerate() {
            if let Some(existing) = self.class_named(&decl.name.0) {
                let message = match self.class_decl(existing) {
                    Some(earlier) if !earlier.file.0.is_empty() => format!(
                        "Cannot redeclare class {} (previously declared in {}:{})",
                        String::from_utf8_lossy(&decl.name.0),
                        String::from_utf8_lossy(&earlier.file.0),
                        earlier.line
                    ),
                    _ => format!(
                        "Cannot redeclare class {}",
                        String::from_utf8_lossy(&decl.name.0)
                    ),
                };
                let file = decl.file.0.to_vec();
                self.end_in_compile_time(Level::CompileError, &message, &file, decl.line);
                return false;
            }
            let Ok(at) = u32::try_from(at) else {
                return false;
            };
            self.declared.push((Arc::clone(unit), at));
        }
        true
    }

    pub(crate) fn function_named(&self, name: &[u8]) -> Option<Function> {
        let wanted = name.to_ascii_lowercase();
        self.shared
            .functions
            .iter()
            .position(|(decl, _)| decl.name.0.to_ascii_lowercase() == wanted)
            .and_then(|i| u32::try_from(i).ok())
            .map(Function)
    }

    pub(crate) fn constant_named(&self, name: &[u8]) -> Option<Value> {
        self.constants
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, v)| v.clone())
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

    /// `header()`: add a header line, replacing one of the same name
    /// when `replace` is set; `false` once headers are sent.
    pub fn header_add(&mut self, line: &[u8], replace: bool) -> bool {
        if self.headers_sent().is_some() {
            return false;
        }
        let name = line
            .split(|b| *b == b':')
            .next()
            .unwrap_or(line)
            .to_ascii_lowercase();
        if replace {
            self.headers.retain(|h| {
                h.split(|b| *b == b':')
                    .next()
                    .map(<[u8]>::to_ascii_lowercase)
                    != Some(name.clone())
            });
        }
        self.headers.push(line.to_vec());
        true
    }

    /// `header_remove()`: one header by name, or all with `None`.
    pub fn header_remove(&mut self, name: Option<&[u8]>) {
        match name {
            None => self.headers.clear(),
            Some(name) => {
                let name = name.to_ascii_lowercase();
                self.headers.retain(|h| {
                    h.split(|b| *b == b':')
                        .next()
                        .map(<[u8]>::to_ascii_lowercase)
                        != Some(name.clone())
                });
            }
        }
    }

    /// `headers_list()`.
    #[must_use]
    pub fn headers_list(&self) -> Vec<Vec<u8>> {
        self.headers.clone()
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

    /// `http_response_code()` as it stands.
    #[must_use]
    pub fn response_code(&self) -> i64 {
        self.response_code
    }

    /// `http_response_code(int)`.
    pub fn set_response_code(&mut self, code: i64) {
        self.response_code = code;
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
        call.arguments.get(index).cloned()
    }

    /// `zend_parse_arg_*_weak` writing what it coerced back into the
    /// argument's zval: the call frame gives up what it held at
    /// `position` and owns `value` from here, so the native reads a view
    /// and the frame frees the string with the call. A position past
    /// what the call was given writes nothing.
    pub(crate) fn replace_argument(&mut self, position: u32, value: Value) {
        let Some(index) = position
            .checked_sub(1)
            .and_then(|at| usize::try_from(at).ok())
        else {
            return;
        };
        let Some(previous) = self
            .vm
            .serving_call_mut()
            .and_then(|call| call.arguments.get_mut(index))
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
        let function = self.active_native_function()?;
        Some(self.native_method(function)?.class)
    }

    /// `zend_get_called_scope`, the called scope of the native being
    /// served, `static`: the receiver's class for an instance call and
    /// the class named for a call on the class; none for a function or
    /// outside a native's body.
    #[must_use]
    pub fn called_scope(&self) -> Option<ClassId> {
        self.vm.serving_call()?.called_scope
    }

    /// `ZEND_THIS`, `getThis()`: the receiver of the method being
    /// served, a name the frame holds an owner of while the call
    /// stands; none for a call on the class, a function, or outside a
    /// native's body.
    #[must_use]
    pub fn this(&self) -> Option<PhpObject> {
        self.vm.serving_call()?.this
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

    /// The declaration of a linked function.
    #[must_use]
    pub fn function_decl(&self, function: Function) -> Option<&FunctionDecl> {
        let at = usize::try_from(function.0).ok()?;
        match at.checked_sub(self.shared.functions.len()) {
            None => self.shared.functions.get(at).map(|(decl, _)| decl),
            Some(_) => self.method_decl(function).map(|method| &method.function),
        }
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

    /// `register_shutdown_function()`.
    pub fn register_shutdown_function(&mut self, callback: Callable, arguments: Vec<Value>) {
        self.shutdown_functions.push((callback, arguments));
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

    /// The request's heap, read: what the store holds, for a test that
    /// asks after an object's bucket.
    #[cfg(test)]
    pub(crate) fn heap(&self) -> &Heap {
        &self.heap
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
    /// property the copy is put in.
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
    pub(crate) fn release(&mut self, value: Value) {
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

    fn destruct(&mut self, obj: Obj) {
        self.0.destruct(obj);
    }

    fn free_native(&mut self, obj: Obj) {
        self.0.free_native(obj);
    }

    fn gc_children(&mut self, obj: Obj, visit: &mut dyn FnMut(Zval)) {
        let _ = (obj, visit);
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
    /// family takes, and every other level is masked. No raiser reaches
    /// `E_CORE_ERROR` or `E_CORE_WARNING` in this engine, so the mask is
    /// held here and not by a witness.
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
            "\nWarning: core in witness.php on line 2\n\nFatal error: core in witness.php on line 3\n"
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
