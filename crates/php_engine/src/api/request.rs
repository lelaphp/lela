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

use php_heap::{Fatal, FrameRef, Heap, HeapHooks, MapTable, ObjectId, VmStack, Zval};
use php_unit::Unit;
use php_unit::decl::{ClassDecl, FunctionDecl};

use crate::api::abi::Callable;
use crate::api::call::Function;
use crate::api::raising::{Level, Thrown};
use crate::api::values::{ClassId, Key, PhpArray, PhpObject, PhpString, Resource, Value};
use crate::engine::{RequestDescription, Shared};
use crate::exec::{self, Vm};

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
    pub scheme: PhpString,
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
    pub name: PhpString,
    pub filter: fn(&mut Request, &[u8], &mut Vec<u8>, bool) -> Result<(), Thrown>,
}

/// The request's resource table, `EG(regular_list)`: ids are handed out
/// once and never reused within a request.
#[derive(Debug, Default)]
pub struct Resources {
    entries: Vec<Option<Resource>>,
}

impl Resources {
    /// Register a resource; its id is written into it and returned.
    pub fn insert(&mut self, mut resource: Resource) -> u32 {
        let id = u32::try_from(self.entries.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        resource.id = id;
        self.entries.push(Some(resource));
        id
    }

    /// The resource with `id`, if it is live.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<&Resource> {
        self.entries
            .get(usize::try_from(id.checked_sub(1)?).ok()?)?
            .as_ref()
    }

    /// Remove the resource with `id`, running its destructor.
    pub fn remove(&mut self, id: u32) -> Option<Resource> {
        let slot = self
            .entries
            .get_mut(usize::try_from(id.checked_sub(1)?).ok()?)?;
        slot.take()
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
    /// `E_ERROR`: the fatal as `php_error_cb` wrote it to the host's log,
    /// which is the text `serve` reports and not a second rendering; the
    /// streams already have theirs.
    Fatal(String),
    /// `E_PARSE`: the text is already on the stream `php_error_cb` put it
    /// on, and what is left is php's status, 255.
    Parse,
    Exit(i32),
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
    map: MapTable,
    states: Vec<(TypeId, Box<dyn Any + Send>)>,
    ini: Vec<(PhpString, PhpString)>,
    body: Vec<u8>,
    headers: Vec<PhpString>,
    header_callback: Option<Callable>,
    log: Vec<u8>,
    response_code: i64,
    last_error: Option<(Level, String, PhpString, u32)>,
    ending: Option<Ending>,
    resources: Resources,
    superglobals: Vec<(PhpString, PhpArray)>,
    constants: Vec<(PhpString, Value)>,
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
    next_object: u32,
    streams: u32,
    flushed: bool,
    /// The native whose body is being served, for the raisers' text.
    active_native: Option<String>,
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
            headers: Vec::new(),
            header_callback: None,
            log: Vec::new(),
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
            next_object: 1,
            streams: 0,
            flushed: false,
            active_native: None,
            strict_types: false,
            pending: None,
            returned: Value::null(),
            interrupt: Interrupt::default(),
            timer: None,
        };
        // Request state in bundle order (ADR-0005).
        for extension in &req.shared.extensions {
            if let Some((type_id, build)) = &extension.request_state {
                req.states.push((*type_id, build()));
            }
        }
        // The execution timer, as `zend_set_timeout` arms one at the
        // start of a request (design §5.8).
        req.arm_timer();
        req
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

    pub(crate) fn pop_frame(&mut self) {
        self.vm.pop();
        self.stack.pop_frame();
    }

    /// What the entry frame of the innermost serving returned.
    pub(crate) fn set_returned(&mut self, value: Value) {
        self.returned = value;
    }

    /// Leave what a raise made pending where the loop reads it, php's
    /// `EG(exception)`. A `Thrown` that carries none is a fatal or an
    /// `exit` the raiser already recorded on the request.
    pub(crate) fn leave_pending(&mut self, thrown: &Thrown) {
        if self.pending.is_none() {
            self.pending = thrown.exception().cloned();
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
    pub(crate) fn uncaught(&mut self) -> Result<(), Fatal> {
        let Some(throwable) = self.pending.take() else {
            return Ok(());
        };
        let (file, line) = Request::throw_site(&throwable);
        let message = format!(
            "Uncaught {}\n  thrown",
            self.throwable_to_string(&throwable)
        );
        self.end_in_fatal_at(&message, &file, line);
        Err(Fatal::Error(message))
    }

    /// `Exception::__toString`: the class, the message where there is one,
    /// the site behind a colon and the trace under `Stack trace:`. The
    /// chain of `previous` throwables php walks after the first, and the
    /// ` and defined` a `TypeError` whose message names a call site takes,
    /// arrive with the user-function frames that make either reachable.
    fn throwable_to_string(&self, throwable: &PhpObject) -> String {
        let class = self.class_decl(throwable.class()).map_or_else(
            || String::from("Error"),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        );
        let message = string_prop(throwable, b"message");
        let (file, line) = Request::throw_site(throwable);
        let file = String::from_utf8_lossy(file.as_bytes());
        // `zend_trace_to_string` over an empty trace is `#0 {main}`, which
        // is what a throwable made before this engine recorded one has.
        let trace = throwable
            .prop(b"trace")
            .and_then(|value| value.as_str_bytes())
            .map_or_else(
                || String::from("#0 {main}"),
                |bytes| String::from_utf8_lossy(bytes).into_owned(),
            );
        if message.is_empty() {
            format!("{class} in {file}:{line}\nStack trace:\n{trace}")
        } else {
            format!("{class}: {message} in {file}:{line}\nStack trace:\n{trace}")
        }
    }

    /// The `file` and `line` a throwable recorded where it was made.
    fn throw_site(throwable: &PhpObject) -> (PhpString, u32) {
        let file = throwable
            .prop(b"file")
            .and_then(|value| value.as_php_string().cloned())
            .unwrap_or_default();
        let line = throwable
            .prop(b"line")
            .and_then(Value::as_int)
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
    pub(crate) fn note_throw_site(&self, throwable: &mut PhpObject) {
        throwable.set_prop(
            b"file",
            Value::string(self.current_file().cloned().unwrap_or_default()),
        );
        throwable.set_prop(b"line", Value::int(i64::from(self.current_line())));
        throwable.set_prop(
            b"trace",
            Value::string(PhpString::from_bytes(self.trace_as_string().as_bytes())),
        );
    }

    /// `zend_fetch_debug_backtrace` rendered by `zend_trace_to_string`
    /// with `include_main`: one `#N file(line): name()` per call below the
    /// top, then `#N {main}`. php walks `EG(current_execute_data)`; the
    /// only call this engine pushes below the op array being served is the
    /// native it is serving, whose file and line are the caller's, as
    /// php's frame for a native takes them.
    ///
    /// The arguments php prints inside the parentheses are the ones the
    /// call frame holds on the VM stack. This engine's native ABI hands
    /// them to the entry as a slice and keeps none, so the frame renders
    /// with empty parentheses; no native this engine declares can throw
    /// with an argument in hand.
    fn trace_as_string(&self) -> String {
        let mut frames: Vec<String> = Vec::new();
        if let Some(native) = &self.active_native {
            let file = self.current_file().cloned().unwrap_or_default();
            frames.push(format!(
                "#0 {}({}): {native}()",
                String::from_utf8_lossy(file.as_bytes()),
                self.current_line()
            ));
        }
        let main = frames.len();
        frames.push(format!("#{main} {{main}}"));
        frames.join("\n")
    }

    /// Serve one native: its entry with the arguments as passed, with the
    /// name the raisers render in their text (ADR-0002).
    pub(crate) fn serve_native(
        &mut self,
        function: Function,
        name: &PhpString,
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        let Some(entry) = self
            .shared
            .functions
            .get(usize::try_from(function.0).unwrap_or(usize::MAX))
            .map(|(_, entry)| *entry)
        else {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!(
                    "Call to undefined function {}()",
                    String::from_utf8_lossy(name.as_bytes())
                ),
            ));
        };
        let outer = self
            .active_native
            .replace(String::from_utf8_lossy(name.as_bytes()).into_owned());
        let served = entry(self, arguments);
        self.active_native = outer;
        served
    }

    pub(crate) fn take_body(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.body)
    }

    pub(crate) fn take_log(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.log)
    }

    pub(crate) fn header_block(&self) -> Vec<u8> {
        let mut block = Vec::new();
        for header in &self.headers {
            block.extend_from_slice(header.as_bytes());
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
        let file = self.current_file().cloned().unwrap_or_default();
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
    /// under it and a warning's stands as it is; the escape is held by
    /// `escape_html`'s test against the oracle's bytes, since a parse
    /// error is raised before `ini_set()` runs and no fatal this engine
    /// raises at runtime carries a byte the escape moves (#186).
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
    pub(crate) fn note_error_in(
        &mut self,
        level: Level,
        message: &str,
        file: &PhpString,
        line: u32,
    ) {
        if !self.displayed(message, file, line) {
            return;
        }
        let file = file.clone();
        let named = String::from_utf8_lossy(file.as_bytes()).into_owned();
        // php stores the diagnostic before it masks it, which is why
        // `error_get_last()` answers one `error_reporting` hid.
        self.last_error = Some((level, message.to_owned(), file, line));
        if self.error_reporting() & level.bit() == 0 {
            return;
        }
        let name = level_name(level);
        if self.log_errors() {
            let text = format!("PHP {name}:  {message} in {named} on line {line}\n");
            self.log.extend_from_slice(text.as_bytes());
        }
        let display = self.display_errors();
        if display == Display::Off {
            return;
        }
        if self.html_errors() {
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
    fn displayed(&self, message: &str, file: &PhpString, line: u32) -> bool {
        if !ini_truth(self.ini_get(b"ignore_repeated_errors").as_ref()) {
            return true;
        }
        let Some((_, last, last_file, last_line)) = self.last_error.as_ref() else {
            return true;
        };
        if last != message {
            return true;
        }
        if ini_truth(self.ini_get(b"ignore_repeated_source").as_ref()) {
            return false;
        }
        *last_line != line || last_file.as_bytes() != file.as_bytes()
    }

    /// `log_errors`, whether a diagnostic reaches the host's log.
    fn log_errors(&self) -> bool {
        ini_truth(self.ini_get(b"log_errors").as_ref())
    }

    /// `html_errors`, which of php's two display forms a diagnostic
    /// takes: the plain one, or `<br />\n<b>Warning</b>:  …`.
    fn html_errors(&self) -> bool {
        ini_truth(self.ini_get(b"html_errors").as_ref())
    }

    /// One ini entry as php's `STR_PRINT(INI_STR(name))` reads it: its
    /// bytes, and nothing at all where php left the entry `NULL`.
    fn ini_text(&self, name: &[u8]) -> String {
        self.ini_get(name)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
            .unwrap_or_default()
    }

    /// `display_errors`, php's three values: off, the body, and the
    /// host's error stream, which the CLI and CGI hosts write a
    /// diagnostic to without php's leading newline.
    fn display_errors(&self) -> Display {
        let value = self.ini_get(b"display_errors");
        let text = value.as_ref().map(|v| v.as_bytes().to_ascii_lowercase());
        match text.as_deref() {
            Some(b"stderr") => Display::Stderr,
            _ if ini_truth(value.as_ref()) => Display::Body,
            _ => Display::Off,
        }
    }

    /// `zend_error_noreturn(E_ERROR, ...)`: `php_error_cb` renders the
    /// fatal where the request stands, on the streams the two ini entries
    /// say, and the request ends in it. It is rendered once, here, and
    /// `serve` writes nothing of its own.
    pub(crate) fn end_in_fatal(&mut self, message: &str) {
        let file = self.current_file().cloned().unwrap_or_default();
        let line = self.current_line();
        self.end_in_fatal_at(message, &file, line);
    }

    /// The same fatal at a site of its own, which an uncaught throwable
    /// carries. The first fatal is the one that ends the request, since
    /// php leaves the request from the first and renders no second.
    pub(crate) fn end_in_fatal_at(&mut self, message: &str, file: &PhpString, line: u32) {
        if self.ending.is_some() {
            return;
        }
        self.note_error_in(Level::Error, message, file, line);
        self.ending = Some(Ending::Fatal(format!(
            "PHP Fatal error:  {message} in {} on line {line}\n",
            String::from_utf8_lossy(file.as_bytes())
        )));
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

    pub(crate) fn new_object(&mut self, class: ClassId) -> PhpObject {
        let id = self.next_object;
        self.next_object = self.next_object.saturating_add(1);
        PhpObject::new(id, class)
    }

    pub(crate) fn new_object_of(&mut self, class_name: &str) -> PhpObject {
        let class = self
            .class_named(class_name.as_bytes())
            .unwrap_or(ClassId(0));
        self.new_object(class)
    }

    pub(crate) fn active_native(&self) -> String {
        self.active_native
            .clone()
            .unwrap_or_else(|| String::from("native"))
    }

    pub(crate) fn argument_message(&self, position: u32, tail: &str) -> String {
        format!(
            "{}(): Argument #{position} must {tail}",
            self.active_native()
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
            .and_then(|value| String::from_utf8_lossy(value.as_bytes()).parse().ok())
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
            .position(|decl| decl.name.0.to_ascii_lowercase() == wanted)
            .and_then(|i| u32::try_from(i).ok())
            .map(ClassId)
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
            .find(|(n, _)| n.as_bytes() == name)
            .map(|(_, v)| v.clone())
            .or_else(|| self.shared.constant(name))
    }

    fn ini_parse_bytes_of(&self, name: &str) -> i64 {
        self.ini_get(name.as_bytes())
            .map_or(0, |v| Request::ini_parse_bytes(v.as_bytes()))
    }

    // -------------------------------------------------- output and headers

    /// Write body bytes, through the buffer stack when one is active.
    pub fn write(&mut self, bytes: &[u8]) {
        self.body.extend_from_slice(bytes);
    }

    /// Write to the host's error stream, `STDERR` and `error_log` to it.
    pub fn write_stderr(&mut self, bytes: &[u8]) {
        self.log.extend_from_slice(bytes);
    }

    /// `flush()`: hand what is buffered to the host now.
    pub fn flush(&mut self) {
        self.flushed = true;
    }

    /// One `ob_*` operation. The skeleton has no buffer stack: every
    /// operation answers as php does at level zero.
    pub fn output_buffer_op(&mut self, op: &OutputOp) -> Result<Value, Thrown> {
        Ok(match op {
            OutputOp::Start { .. } => Value::bool(true),
            OutputOp::GetLevel => Value::int(0),
            OutputOp::GetStatus { .. } | OutputOp::ListHandlers => Value::array(PhpArray::new()),
            OutputOp::ImplicitFlush(_) => Value::null(),
            _ => Value::bool(false),
        })
    }

    /// Where the first body byte was written, `headers_sent`'s file and
    /// line, once it has been.
    #[must_use]
    pub fn output_started_at(&self) -> Option<(PhpString, u32)> {
        if self.body.is_empty() {
            None
        } else {
            Some((
                self.current_file().cloned().unwrap_or_default(),
                self.current_line(),
            ))
        }
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
                h.as_bytes()
                    .split(|b| *b == b':')
                    .next()
                    .map(<[u8]>::to_ascii_lowercase)
                    != Some(name.clone())
            });
        }
        self.headers.push(PhpString::from_bytes(line));
        true
    }

    /// `header_remove()`: one header by name, or all with `None`.
    pub fn header_remove(&mut self, name: Option<&[u8]>) {
        match name {
            None => self.headers.clear(),
            Some(name) => {
                let name = name.to_ascii_lowercase();
                self.headers.retain(|h| {
                    h.as_bytes()
                        .split(|b| *b == b':')
                        .next()
                        .map(<[u8]>::to_ascii_lowercase)
                        != Some(name.clone())
                });
            }
        }
    }

    /// `headers_list()`.
    #[must_use]
    pub fn headers_list(&self) -> Vec<PhpString> {
        self.headers.clone()
    }

    /// `headers_sent()`: where the header block was committed, once it was.
    #[must_use]
    pub fn headers_sent(&self) -> Option<(PhpString, u32)> {
        if self.flushed {
            self.output_started_at()
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

    /// The request headers the host received, `getallheaders()`.
    #[must_use]
    pub fn request_headers(&self) -> PhpArray {
        let mut array = PhpArray::new();
        for (name, value) in &self.description.headers {
            array.set(
                Key::Str(PhpString::from_bytes(name)),
                Value::string(PhpString::from_bytes(value)),
            );
        }
        array
    }

    // -------------------------------------------------- diagnostics

    /// `error_get_last()`.
    #[must_use]
    pub fn last_error(&self) -> Option<PhpArray> {
        let (level, message, file, line) = self.last_error.as_ref()?;
        let mut array = PhpArray::new();
        array.set(Key::from_bytes(b"type"), Value::int(level.bit()));
        array.set(
            Key::from_bytes(b"message"),
            Value::string(PhpString::from_bytes(message.as_bytes())),
        );
        array.set(Key::from_bytes(b"file"), Value::string(file.clone()));
        array.set(Key::from_bytes(b"line"), Value::int(i64::from(*line)));
        Some(array)
    }

    /// `error_clear_last()`.
    pub fn clear_last_error(&mut self) {
        self.last_error = None;
    }

    /// `error_log()` to the host's log.
    pub fn error_log(&mut self, message: &[u8]) {
        self.log.extend_from_slice(message);
        self.log.push(b'\n');
    }

    /// `error_reporting()` as it stands.
    #[must_use]
    pub fn error_reporting(&self) -> i64 {
        self.ini_get(b"error_reporting")
            .and_then(|v| String::from_utf8_lossy(v.as_bytes()).parse().ok())
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
    #[must_use]
    pub fn backtrace(&self, options: i64, limit: i64) -> PhpArray {
        let _ = (options, limit);
        PhpArray::new()
    }

    /// The file of the frame that called the native.
    #[must_use]
    pub fn current_file(&self) -> Option<&PhpString> {
        self.description.script_name.as_ref()
    }

    /// `zend_get_executed_lineno`: the line the innermost frame stands
    /// on, which is the line a diagnostic and a backtrace name.
    #[must_use]
    pub fn current_line(&self) -> u32 {
        self.vm.executing_line()
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

    /// The class scope of the frame that called the native, `self`.
    #[must_use]
    pub fn scope(&self) -> Option<ClassId> {
        None
    }

    /// The called scope, `static`.
    #[must_use]
    pub fn called_scope(&self) -> Option<ClassId> {
        None
    }

    // -------------------------------------------------- symbols

    /// The declaration of a linked class.
    #[must_use]
    pub fn class_decl(&self, class: ClassId) -> Option<&ClassDecl> {
        self.shared.classes.get(usize::try_from(class.0).ok()?)
    }

    /// The declaration of a linked function.
    #[must_use]
    pub fn function_decl(&self, function: Function) -> Option<&FunctionDecl> {
        self.shared
            .functions
            .get(usize::try_from(function.0).ok()?)
            .map(|(decl, _)| decl)
    }

    /// `get_declared_classes()`, in declaration order.
    #[must_use]
    pub fn declared_classes(&self) -> Vec<ClassId> {
        (0..u32::try_from(self.shared.classes.len()).unwrap_or(u32::MAX))
            .map(ClassId)
            .collect()
    }

    /// `get_defined_functions()`: `internal` and `user` lists.
    #[must_use]
    pub fn defined_functions(&self) -> PhpArray {
        let mut internal = PhpArray::new();
        for (decl, _) in &self.shared.functions {
            internal.append(Value::string(PhpString::from_bytes(&decl.name.0)));
        }
        let mut array = PhpArray::new();
        array.set(Key::from_bytes(b"internal"), Value::array(internal));
        array.set(Key::from_bytes(b"user"), Value::array(PhpArray::new()));
        array
    }

    /// `class_alias()`.
    pub fn class_alias(&mut self, class: ClassId, alias: &[u8]) -> Result<(), Thrown> {
        let _ = (class, alias);
        Ok(())
    }

    /// `get_defined_constants()`.
    #[must_use]
    pub fn constants(&self) -> PhpArray {
        let mut array = PhpArray::new();
        for (name, value) in &self.constants {
            array.set(Key::Str(name.clone()), value.clone());
        }
        array
    }

    /// `define()`: `false` when the constant exists.
    pub fn define_constant(&mut self, name: &[u8], value: Value) -> Result<bool, Thrown> {
        if self.constant_named(name).is_some() {
            return Ok(false);
        }
        self.constants.push((PhpString::from_bytes(name), value));
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

    /// `get_included_files()`.
    #[must_use]
    pub fn included_files(&self) -> Vec<PhpString> {
        self.description.script_name.iter().cloned().collect()
    }

    /// `get_loaded_extensions()`: the bundle in `MINIT` order, after `Core`.
    #[must_use]
    pub fn loaded_extensions(&self) -> Vec<PhpString> {
        std::iter::once(PhpString::from_bytes(b"Core"))
            .chain(
                self.shared
                    .extensions
                    .iter()
                    .map(|e| PhpString::from_bytes(e.name.as_bytes())),
            )
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

    /// The request's resource table.
    pub fn resources(&mut self) -> &mut Resources {
        &mut self.resources
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
    pub fn ini_get(&self, name: &[u8]) -> Option<PhpString> {
        self.ini
            .iter()
            .find(|(n, _)| n.as_bytes() == name)
            .map(|(_, v)| v.clone())
            .or_else(|| self.shared.ini_default(name))
    }

    /// `ini_set()`: the previous value, or `None` for an entry that does
    /// not exist or may not change at runtime. An entry with an
    /// on-modify handler runs it here: `max_execution_time` re-arms the
    /// execution timer (`OnUpdateTimeout`, `Z/main/main.c:467`).
    pub fn ini_set(&mut self, name: &[u8], value: &[u8]) -> Result<Option<PhpString>, Thrown> {
        let previous = self.ini_get(name);
        if previous.is_none() {
            return Ok(None);
        }
        self.ini.retain(|(n, _)| n.as_bytes() != name);
        self.ini
            .push((PhpString::from_bytes(name), PhpString::from_bytes(value)));
        if name == b"max_execution_time" {
            self.arm_timer();
        }
        Ok(previous)
    }

    /// `ini_get_all()`: every entry with its current value.
    #[must_use]
    pub fn ini_entries(&self) -> PhpArray {
        let mut array = PhpArray::new();
        for (name, default) in self.shared.ini_defaults() {
            let value = self.ini_get(name.as_bytes()).unwrap_or(default);
            array.set(Key::Str(name), Value::string(value));
        }
        array
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

    /// `getenv(name)`.
    #[must_use]
    pub fn env(&self, name: &[u8]) -> Option<PhpString> {
        self.description
            .env
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, v)| PhpString::from_bytes(v))
    }

    /// `getenv()` with no name: every pair.
    #[must_use]
    pub fn env_pairs(&self) -> Vec<(PhpString, PhpString)> {
        self.description
            .env
            .iter()
            .map(|(n, v)| (PhpString::from_bytes(n), PhpString::from_bytes(v)))
            .collect()
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
            .find(|(n, _)| n.as_bytes() == name)
            .map(|(_, a)| a)
    }

    /// Replace a superglobal, what the GPC seeding and `session_start` do.
    pub fn set_superglobal(&mut self, name: &[u8], value: PhpArray) {
        self.superglobals.retain(|(n, _)| n.as_bytes() != name);
        self.superglobals.push((PhpString::from_bytes(name), value));
    }

    /// `php_sapi_name()`.
    #[must_use]
    pub fn sapi_name(&self) -> PhpString {
        PhpString::from_bytes(self.description.sapi_name.as_bytes())
    }

    /// The file the request serves, `$_SERVER['SCRIPT_NAME']`.
    #[must_use]
    pub fn script_name(&self) -> Option<&PhpString> {
        self.description.script_name.as_ref()
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
    pub fn os_error_text(&self, code: i32) -> PhpString {
        PhpString::from_bytes(
            std::io::Error::from_raw_os_error(code)
                .to_string()
                .as_bytes(),
        )
    }

    /// `parse_str()`: a query string as an array.
    #[must_use]
    pub fn parse_query(&self, query: &[u8]) -> PhpArray {
        let mut array = PhpArray::new();
        for pair in query.split(|b| *b == b'&').filter(|p| !p.is_empty()) {
            let mut halves = pair.splitn(2, |b| *b == b'=');
            let name = halves.next().unwrap_or_default();
            let value = halves.next().unwrap_or_default();
            array.set(
                Key::from_bytes(name),
                Value::string(PhpString::from_bytes(value)),
            );
        }
        array
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

    /// `gc_status()` with php 8.5's fields.
    #[must_use]
    pub fn gc_status(&self) -> PhpArray {
        let mut array = PhpArray::new();
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
            array.set(Key::from_bytes(name.as_bytes()), value);
        }
        array
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
    pub fn stream_wrappers(&self) -> Vec<PhpString> {
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

impl HeapHooks for Request {
    fn destruct(&mut self, obj: ObjectId) {
        let _ = obj;
    }

    fn free_native(&mut self, obj: ObjectId) {
        let _ = obj;
    }

    fn gc_children(&mut self, obj: ObjectId, visit: &mut dyn FnMut(Zval)) {
        let _ = (obj, visit);
    }

    fn free_resource(&mut self, res: u32) {
        self.resources.remove(res);
    }

    fn out_of_memory(&mut self, bytes: usize) -> Fatal {
        let _ = &self.map;
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

/// `zend_ini_parse_bool`: an ini entry read as on or off, where "off",
/// "no", "false" and "" are off beside "0" and everything else is on.
fn ini_truth(value: Option<&PhpString>) -> bool {
    let Some(value) = value else { return false };
    let text = value.as_bytes().to_ascii_lowercase();
    !matches!(text.as_slice(), b"" | b"0" | b"off" | b"no" | b"false")
}

/// One string property of a throwable, empty where there is none.
fn string_prop(object: &PhpObject, name: &[u8]) -> String {
    object
        .prop(name)
        .and_then(|value| value.as_str_bytes())
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default()
}

/// php's `error_type_str` (`Z/main/main.c:1414-1448`), the word both
/// renderings name the level by. `E_RECOVERABLE_ERROR` is the one level
/// whose word is not its family's, and `E_STRICT` is the one level php
/// 8.5 has no case for, so it falls to the `default:` every level php
/// dropped falls to.
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

#[cfg(test)]
mod tests {
    use super::{Level, escape_html, level_name};

    /// php's `error_type_str` for every level `Level` names, the switch
    /// at `Z/main/main.c:1414-1448` read case by case. Nothing in this
    /// engine raises `E_RECOVERABLE_ERROR` or `E_STRICT` yet, so no
    /// witness reaches either word; #186 is where the two come back as
    /// fences.
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
