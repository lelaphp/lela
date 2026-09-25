//! `Engine` and `serve` (design §5.14): the process-wide, `Sync` engine
//! and the one request transaction every host crosses. It compiles the
//! source through `php_compile`, builds the request with every
//! extension's request state, serves `rinit`, hands the main op array's
//! entry frame to the tier-bound loop, serves `rshutdown` in reverse,
//! renders what php renders, and hands the bytes to the sink.
//!
//! The engine's own declarations go in first: the natives design §13
//! moves out of `ext/standard` and the throwable classes Zend declares,
//! so an engine with no extensions serves a program that uses Zend alone
//! (design §4.10).
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use php_compile::{CompileError, StackLimit};
use php_heap::{Fatal, FrameRef, Intern, Strings, Zval};
use php_unit::decl::{ClassDecl, ConstDecl, ConstExpr, FunctionDecl, MethodDecl, Name};
use php_unit::{FramelessEntry, Unit, frameless_table};

use crate::api::abi::{Cache, Entry, Extension, NativeNew, ObjectHandlers, StartupIni};
use crate::api::request::{Aside, Ending, ExtensionId, Request};
use crate::api::values::{ClassId, Value};
use crate::exec;
use crate::streams::Standard;
use crate::tier::{Nop, Tier, execute, reenter, resume};

/// Everything a host knows about one request before it starts (design
/// §4.3 seam 4): one plain `Send` value, handed over once.
#[derive(Debug, Clone, Default)]
pub struct RequestDescription {
    pub argv: Vec<Vec<u8>>,
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    pub cwd: PathBuf,
    pub server: Vec<(Vec<u8>, Vec<u8>)>,
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub body: Vec<u8>,
    pub sapi_name: String,
    pub is_web: bool,
    /// The served file's name, for `$_SERVER['SCRIPT_NAME']` and error text.
    pub script_name: Option<Vec<u8>>,
    /// The source to serve, when the host has it in hand (`php -r`, the
    /// in-process tests); else the file is read by its name.
    pub source: Option<Vec<u8>>,
    /// The CLI's standard handles, which `do_cli` registers after
    /// `php_request_startup` and php's other hosts have none of; `None`
    /// registers nothing.
    pub file_handles: Option<FileHandles>,
}

/// What `do_cli` registers for a request it serves a program in
/// (`Z/sapi/cli/php_cli.c:926-980` at 8.5.9), and the host's fds under
/// it. `PHP_CLI_PROCESS_TITLE` first, then `cli_register_file_handles`
/// (`:519`): `php://stdin`, `php://stdout` and `php://stderr` opened in
/// that order, which makes them the request's resources 1, 2 and 3, and
/// the constants `STDIN`, `STDOUT` and `STDERR` on them, each under
/// `Core`. `do_cli`'s `-l` registers no stream, so a host leaves this
/// `None` for a lint, and php's CGI and `FastCGI` hosts have none of it.
///
/// What is written to the streams goes to the sink, `STDOUT` past the
/// `php_output` layer and `STDERR` onto its error stream (design
/// revision 16), and `STDIN` reads the request's body. The fds are what
/// `fstat` and `ftruncate` on the three ask, as php's `STDIO` kind asks
/// `fstat(2)` and `ftruncate(2)` of fds 0, 1 and 2; a host with no fd for
/// one of them hands `None`, which `fstat(2)` and `ftruncate(2)` answer
/// as they answer a closed fd, so both answer `false`.
#[derive(Debug, Clone)]
pub struct FileHandles {
    fds: Arc<[Option<std::os::fd::OwnedFd>; 3]>,
}

impl FileHandles {
    /// The three over the fds the host hands, closed when the last
    /// description holding them is dropped; a host hands copies of its
    /// own, as `dup(2)` makes them.
    #[must_use]
    pub fn new(
        stdin: Option<std::os::fd::OwnedFd>,
        stdout: Option<std::os::fd::OwnedFd>,
        stderr: Option<std::os::fd::OwnedFd>,
    ) -> FileHandles {
        FileHandles {
            fds: Arc::new([stdin, stdout, stderr]),
        }
    }

    /// The fd under one of the three, `None` where the host handed none.
    pub(crate) fn fd(&self, standard: Standard) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        let [stdin, stdout, stderr] = &*self.fds;
        let fd = match standard {
            Standard::Stdin => stdin,
            Standard::Stdout => stdout,
            Standard::Stderr => stderr,
        };
        fd.as_ref().map(AsFd::as_fd)
    }
}

/// Where a request's bytes go (design §4.3 seam 4): the engine owns the
/// header list, the status and the commit moment; a sink only writes.
pub trait OutputSink {
    /// Body bytes.
    fn write(&mut self, bytes: &[u8]);
    /// The host may push what it has.
    fn flush(&mut self);
    /// The rendered header block, once, before the first body byte.
    fn send_headers(&mut self, block: &[u8]);
    /// One entry for the host's log, whole: what `php_log_err` was
    /// given, a diagnostic in its log form with its stack trace under
    /// it where it has one, or `error_log()`'s message as given, so it
    /// may carry newlines inside it and ends in none. The host ends it:
    /// php's CLI writes `"%s\n"` to stderr and a file log writes
    /// `[date] ` before it and `PHP_EOL` after.
    fn log(&mut self, entry: &[u8]);
    /// Bytes for the host's error stream, as given: what php writes to
    /// the process's `stderr` past the host, the `display_errors=stderr`
    /// form of a diagnostic with its `\n` (`Z/main/main.c:1484`) and,
    /// when streams exist, what the `STDERR` stream constant is given.
    /// Where the log goes is the host's: php's CLI puts it on this same
    /// stream, and a host with an `error_log` file parts the two, so a
    /// diagnostic is logged and displayed in that order on one stream
    /// and in no order across two (#216).
    fn write_stderr(&mut self, bytes: &[u8]);
}

/// What `serve` reports of one request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Observables {
    pub exit_code: i32,
    pub headers_sent: bool,
    /// The fatal's rendered text, when the request ended in one.
    pub fatal: Option<String>,
}

/// The loop bound to the engine's tier, `execute::<T>` for one `T`.
type Loop = fn(&mut Request, FrameRef) -> Result<(), Fatal>;

/// What the unit cache holds for one file: the published unit, the
/// refusal the compiler gave for it, or nothing at all for a file that
/// could not be read when it was asked for.
pub(crate) type Cached = Option<Result<Arc<Unit>, CompileError>>;

/// What the unit cache is keyed by (design §6.3): the file's name and
/// what says it has not changed under the engine, php's realpath with
/// its `(mtime, size)`. A file edited in place is a key of its own, so
/// what was cached for the old bytes is never served for the new.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UnitKey {
    path: Vec<u8>,
    /// What the system reports as the file's last write, php's
    /// `zend_stat_t.st_mtime`.
    modified: Option<Duration>,
    size: u64,
}

/// php's `opcache.max_accelerated_files` default: the units one engine
/// keeps before it drops half of them.
const MAX_ACCELERATED_FILES: usize = 10_000;

/// What every request shares: the bundle, the linked declarations, the
/// engine states, the ini defaults and the loop (design §5.14).
pub(crate) struct Shared {
    pub(crate) extensions: Vec<Extension>,
    /// `CG(function_table)`'s natives, the engine's own and then each
    /// extension's in bundle order, less what the startup ini's
    /// `disable_functions` took out of it ([`function_table`]).
    pub(crate) functions: Vec<(FunctionDecl, Entry)>,
    /// Each native's `internal_function.module`, one row per row of
    /// `functions`: the extension that declared it, or none for one the
    /// engine declares itself.
    pub(crate) function_modules: Vec<Option<ExtensionId>>,
    /// `zend_flf_functions` (`Z/Zend/zend_API.c:3115-3133`): the natives
    /// with a frameless entry, one row per arity in registration order,
    /// which a `FRAMELESS_ICALL_*`'s `extended_value` indexes. Computed
    /// from `functions` by the one function the compiler numbers by.
    pub(crate) frameless: Vec<FramelessEntry>,
    /// The classes Zend declares and then each extension's, in bundle
    /// order, each with its methods on it: what a `ClassId` below the
    /// request's own declarations indexes.
    pub(crate) classes: Vec<ClassDecl>,
    /// Each native class's handler table (design §5.4), one row per row
    /// of `classes`: the table the class declared, or none where it
    /// takes its parent's, `Std`'s at a root.
    pub(crate) handlers: Vec<Option<&'static dyn ObjectHandlers>>,
    /// Each native class's `create_object` where its objects carry a
    /// native payload (design §5.4), one row per row of `classes`: the
    /// `NativeNew` the class declared, or none where it takes its
    /// parent's and `zend_objects_new` at a root.
    pub(crate) natives: Vec<Option<NativeNew>>,
    /// Each native method's entry, in the order the classes declare
    /// them and then the trampolines': a `Function` past `functions`
    /// indexes here.
    pub(crate) methods: Vec<NativeMethod>,
    /// The declaration of each trampoline, one row per
    /// [`MethodAt::Trampoline`]: a method a class's `get_method`
    /// synthesises, which `ce->function_table` has no row for, so no
    /// lookup by name and no rendering finds it.
    pub(crate) trampolines: Vec<MethodDecl>,
    constants: Vec<ConstDecl>,
    pub(crate) engine_states: Vec<Box<dyn Any + Send + Sync>>,
    ini: Vec<(Vec<u8>, Vec<u8>)>,
    /// The constants' values, the string ones interned into the engine's
    /// table as `REGISTER_STRING_CONSTANT` interns php's at `MINIT`, so
    /// a request reading one allocates nothing and counts nothing.
    constant_values: Vec<(Box<[u8]>, Zval)>,
    /// `PHP_SAPI`'s value under each host's name, interned at build with
    /// the other constants, so a request that reads the name its host
    /// gave it allocates nothing ([`crate::constants::HOSTS`]).
    sapi_names: Vec<(Box<[u8]>, Zval)>,
    entry: Loop,
    /// The same loop for a serving inside a serving, `zend_call_function`
    /// over a user function: what [`Request::serve_user`] enters a
    /// destructor's or a callback's frame through, bound to the same
    /// tier as `entry`.
    pub(crate) reenter: Loop,
    /// The same loop over a generator's frame the resumption put back on
    /// the stack, `zend_generator_resume`'s `zend_execute_ex`, bound to
    /// the same tier (design §5.11, [`crate::tier::resume`]).
    pub(crate) resume: Loop,
    /// The unit cache (design §6.3): what a file compiled to, held for
    /// every request that asks for the same file unchanged. A source the
    /// host handed over is not in it, because php caches neither `-r`
    /// nor `eval`. It stands above the table its units' constants are
    /// interned into, so a dropped engine drops its units first.
    units: Cache<UnitKey, Cached>,
    /// The engine's interned strings (design §5.2, §5.14): what a unit's
    /// constant strings enter as it is published, grown here alone and
    /// read by every request thread with nothing taken.
    strings: Mutex<Strings>,
    /// `CG(map_ptr_last)`: how many map pointers the engine has handed
    /// out (design §5.14), which is the next one's offset. Every op
    /// array of a unit takes one as the unit is published
    /// (`ZEND_MAP_PTR_NEW` at `zend_persist_op_array`,
    /// `Z/ext/opcache/zend_persist.c:715`), and a request's map table
    /// grows to what its units index.
    pub(crate) map_ptr_last: AtomicU32,
}

/// `ZEND_MAP_PTR_NEW` over every op array of `unit`, at its publication:
/// each takes the next offset the engine has, so a request serving the
/// unit finds each op array's runtime cache at its own entry
/// ([`exec::init_runtime_cache`]). A unit compiled for one request,
/// from source the host handed over, takes its offsets here too: php
/// gives such an op array a cache on the heap (`ZEND_ACC_HEAP_RT_CACHE`)
/// and no offset, so its count does not move for it, where this one
/// does.
pub(crate) fn assign_map_ptrs(unit: &mut Unit, map_ptr_last: &AtomicU32) {
    let bodies = std::iter::once(&mut unit.main)
        .chain(unit.functions.iter_mut().map(|(_, code)| code))
        .chain(unit.dynamic.iter_mut().map(|(_, code)| code))
        .chain(unit.methods.iter_mut().map(|method| &mut method.body));
    for code in bodies {
        code.runtime_cache = map_ptr_last.fetch_add(1, Ordering::Relaxed);
    }
}

/// One native method's entry, `zend_internal_function` on a class:
/// which class, where its declaration stands, and the function served
/// for it.
#[derive(Clone, Copy)]
pub(crate) struct NativeMethod {
    pub(crate) class: ClassId,
    pub(crate) at: MethodAt,
    pub(crate) entry: Entry,
}

/// Where a native method's declaration stands: among the class's own,
/// php's `ce->function_table`, which is what a lookup by name walks and
/// a rendering lists; or among the engine's trampolines, a method the
/// class's `get_method` answers that no table carries
/// (`ZEND_ACC_CALL_VIA_HANDLER`). `zend_get_closure_invoke_method`
/// makes one for `Closure::__invoke`, which the stub declares nowhere.
#[derive(Clone, Copy)]
pub(crate) enum MethodAt {
    /// The index among `ce->function_table`'s own declarations.
    Declared(u32),
    /// The index among [`Shared::trampolines`].
    Trampoline(u32),
}

/// `zend_register_internal_enum`'s `zend_register_functions` over the
/// enum methods: a native enum takes `cases`, and `from` and `tryFrom`
/// for a backed one, after the methods its stub declares, served by the
/// entries every enum's are ([`crate::enums::EnumMethod`]).
fn link_enum_methods(decl: &mut ClassDecl, class: ClassId, methods: &mut Vec<NativeMethod>) {
    if decl.kind != php_unit::decl::ClassKind::Enum {
        return;
    }
    for method in php_unit::enum_methods(decl.backing) {
        let lowered = method.function.name.0.to_ascii_lowercase();
        let Some(served) = crate::enums::EnumMethod::of(&lowered, decl.backing) else {
            continue;
        };
        let index = u32::try_from(decl.methods.len()).unwrap_or(u32::MAX);
        decl.methods.push(method);
        methods.push(NativeMethod {
            class,
            at: MethodAt::Declared(index),
            entry: served.entry(),
        });
    }
}

/// The classes the engine links: Zend's own and then each extension's,
/// in bundle order, each with its methods, handler table and
/// `create_object`, the constants later extensions add to them, and the
/// trampolines.
struct LinkedClasses {
    classes: Vec<ClassDecl>,
    handlers: Vec<Option<&'static dyn ObjectHandlers>>,
    natives: Vec<Option<NativeNew>>,
    methods: Vec<NativeMethod>,
    trampolines: Vec<MethodDecl>,
}

/// [`LinkedClasses`] for `extensions`.
fn link_classes(extensions: &[Extension]) -> LinkedClasses {
    // `zend_register_internal_class_with_flags` and `zend_register_functions`
    // over the class's methods: each declaration takes its methods,
    // and each method's entry is numbered in the order it is met.
    let mut zend = crate::interfaces::classes();
    zend.extend(crate::classes::zend_classes());
    zend.extend(crate::fiber::classes());
    let mut classes: Vec<ClassDecl> = Vec::new();
    let mut handlers = Vec::new();
    let mut natives = Vec::new();
    let mut methods = Vec::new();
    for (decl, entries, table, native) in zend
        .iter()
        .chain(extensions.iter().flat_map(|e| e.classes.iter()))
    {
        handlers.push(*table);
        natives.push(*native);
        let mut decl = decl.clone();
        let class = ClassId(u32::try_from(classes.len()).unwrap_or(u32::MAX));
        for (method, entry) in entries {
            let index = u32::try_from(decl.methods.len()).unwrap_or(u32::MAX);
            decl.methods.push(method.clone());
            methods.push(NativeMethod {
                class,
                at: MethodAt::Declared(index),
                entry: *entry,
            });
        }
        link_enum_methods(&mut decl, class, &mut methods);
        classes.push(decl);
    }
    link_class_constants(&mut classes, extensions);
    let trampolines = link_trampolines(&classes, extensions, &mut methods);
    LinkedClasses {
        classes,
        handlers,
        natives,
        methods,
        trampolines,
    }
}

/// `zend_declare_class_constant_ex` over a class another extension
/// registered, from a later `MINIT`: each extension's
/// [`ExtensionBuilder::class_constants`] appended to the class it names,
/// in bundle order.
///
/// [`ExtensionBuilder::class_constants`]: crate::api::abi::ExtensionBuilder::class_constants
fn link_class_constants(classes: &mut [ClassDecl], extensions: &[Extension]) {
    for (class_name, constants) in extensions.iter().flat_map(|e| e.class_constants.iter()) {
        if let Some(decl) = classes
            .iter_mut()
            .find(|decl| decl.name.0.eq_ignore_ascii_case(&class_name.0))
        {
            decl.constants.extend(constants.iter().cloned());
        }
    }
}

/// `zend_register_closure_ce` leaves `__invoke` off `Closure`'s table
/// and `zend_closure_get_method` answers it on the lookup: every
/// trampoline [`crate::classes::trampolines`] names is numbered past
/// the declared methods, so a `Function` still names one row of
/// `methods`, and its declaration stands beside the linked table
/// rather than in it. A row naming a class nothing declares is left
/// unlinked, as a method of an absent class would be.
fn link_trampolines(
    classes: &[ClassDecl],
    extensions: &[Extension],
    methods: &mut Vec<NativeMethod>,
) -> Vec<MethodDecl> {
    let mut trampolines = Vec::new();
    let own = crate::classes::trampolines();
    let laid = extensions
        .iter()
        .flat_map(|e| e.trampolines.iter().cloned());
    for (class_name, method, entry) in own.into_iter().chain(laid) {
        let Some(at) = classes
            .iter()
            .position(|decl| decl.name.0.eq_ignore_ascii_case(&class_name.0))
        else {
            continue;
        };
        let index = u32::try_from(trampolines.len()).unwrap_or(u32::MAX);
        methods.push(NativeMethod {
            class: ClassId(u32::try_from(at).unwrap_or(u32::MAX)),
            at: MethodAt::Trampoline(index),
            entry,
        });
        trampolines.push(method);
    }
    trampolines
}

/// Where a unit is compiled from, as the request describes it: the name
/// the unit and its diagnostics carry, `CG(compiled_filename)`, and the
/// working directory `__DIR__` folds to where that name has no directory
/// (`VCWD_GETCWD`, the request's own under ZTS), which is
/// [`RequestDescription::cwd`] for a request and the process's for a
/// unit compiled outside one ([`Engine::unit`]).
#[derive(Clone, Copy)]
struct Origin<'a> {
    filename: &'a [u8],
    cwd: &'a Path,
}

/// What the engine has declared, as the compiler reads it: php's
/// `CG(function_table)`, `CG(class_table)` and the persistent constants,
/// each a view of the engine's own table.
#[derive(Clone, Copy, Default)]
pub(crate) struct Declared<'a> {
    pub(crate) constants: &'a [ConstDecl],
    pub(crate) functions: &'a [(FunctionDecl, Entry)],
    pub(crate) classes: &'a [ClassDecl],
}

impl Shared {
    /// The tables the compiler reads, as [`Declared`].
    pub(crate) fn declared(&self) -> Declared<'_> {
        Declared {
            constants: &self.constants,
            functions: &self.functions,
            classes: &self.classes,
        }
    }

    /// The engine's own constants, Core's, in php's order
    /// ([`crate::constants::core_constants`]).
    fn zend_constants() -> Vec<ConstDecl> {
        crate::constants::core_constants()
    }

    /// One source compiled with its constant strings interned into the
    /// engine's table, which is what publication is (design §5.2, §6.3):
    /// opcache's persist walks an op array's constants and re-interns
    /// each into the shared table (`Z/ext/opcache/zend_persist.c:525-540`
    /// over `Z/ext/opcache/ZendAccelerator.c:515-575`), and compiling
    /// into that table reaches the same published unit with one
    /// allocation instead of two.
    fn publish(
        &self,
        source: &[u8],
        origin: Origin<'_>,
        stack: StackLimit,
    ) -> Result<Arc<Unit>, CompileError> {
        let mut strings = match self.strings.lock() {
            Ok(strings) => strings,
            // The table is append-only and every string in it was
            // finished before it was published, so what a thread that
            // ended holding the lock left is consistent.
            Err(poisoned) => poisoned.into_inner(),
        };
        compile(
            source,
            origin,
            self.compile_ini(),
            self.declared(),
            stack,
            &mut *strings,
            &self.map_ptr_last,
        )
    }

    /// `zend_call_stack_init` on the calling thread, from the startup
    /// ini's `zend.max_allowed_stack_size` and `zend.reserved_stack_size`
    /// (`Z/Zend/zend.c:277-279`), as php runs it once per thread at
    /// `zend_post_startup` and at each new thread's start: what a request
    /// reads at its start, and what a unit compiled outside one is held
    /// to on the thread that asked for it.
    pub(crate) fn stack_limit(&self) -> StackLimit {
        let bytes = |name: &[u8]| self.ini_default(name).map_or(0, Request::ini_parse_bytes);
        StackLimit::init(
            bytes(b"zend.max_allowed_stack_size"),
            u64::try_from(bytes(b"zend.reserved_stack_size")).unwrap_or(0),
        )
    }

    /// The compiler's ini as the startup one has it, which a unit bound
    /// for the cache is compiled under; a request compiles its own
    /// source under its own ([`CompileIni`]).
    fn compile_ini(&self) -> CompileIni {
        CompileIni {
            precision: self
                .ini_default(b"precision")
                .and_then(|value| String::from_utf8_lossy(value).parse().ok())
                .unwrap_or(14),
            short_open_tag: self
                .ini_default(b"short_open_tag")
                .is_none_or(Request::ini_parse_bool),
        }
    }

    /// The unit for the file `path` names: compiled and published once,
    /// and shared by every request that asks for it afterwards (design
    /// §6.3). A file the engine cannot stat is no unit at all, which is
    /// the `None` `serve` serves nothing for.
    pub(crate) fn cached(&self, path: &[u8], cwd: &Path, stack: StackLimit) -> Cached {
        let name = String::from_utf8_lossy(path).into_owned();
        let described = std::fs::metadata(&name).ok()?;
        let key = UnitKey {
            path: path.to_vec(),
            modified: described
                .modified()
                .ok()
                .and_then(|at| at.duration_since(UNIX_EPOCH).ok()),
            size: described.len(),
        };
        let held = self.units.get_or_build(key, |_| {
            let source = std::fs::read(&name).ok()?;
            Some(self.publish(
                &source,
                Origin {
                    filename: path,
                    cwd,
                },
                stack,
            ))
        });
        Cached::clone(&held)
    }

    pub(crate) fn ini_default(&self, name: &[u8]) -> Option<&[u8]> {
        self.ini
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, v)| v.as_slice())
    }

    pub(crate) fn ini_defaults(&self) -> &[(Vec<u8>, Vec<u8>)] {
        &self.ini
    }

    /// The value of the constant `name` as the bundle declared it, or as
    /// the engine's own table has it: a value nothing owns, since the
    /// string ones are the engine's interned strings.
    /// `PHP_SAPI` for a request its host named `sapi`: the string the
    /// engine interned for that name at build, or, for a name no host
    /// of [`crate::constants::HOSTS`] has, one interned into the
    /// engine's table the first time a request asks, where every later
    /// request finds it.
    pub(crate) fn sapi_constant(&self, sapi: &[u8]) -> Option<Value> {
        if let Some((_, z)) = self.sapi_names.iter().find(|(n, _)| n.as_ref() == sapi) {
            return Some(exec::value_of(*z));
        }
        let mut strings = match self.strings.lock() {
            Ok(strings) => strings,
            Err(poisoned) => poisoned.into_inner(),
        };
        let interned = strings.intern(sapi).ok()?;
        Some(exec::value_of(Zval::string(interned)))
    }

    pub(crate) fn constant(&self, name: &[u8]) -> Option<Value> {
        self.constant_values
            .iter()
            .find(|(n, _)| n.as_ref() == name)
            .map(|(_, z)| exec::value_of(*z))
    }
}

/// The core ini entries and their defaults: php's `main.c` table and
/// six entries of `Zend/zend.c`'s, the two a throwable's trace reads,
/// the two the stack limit reads (`Shared::stack_limit`),
/// `zend.enable_gc`, which turns the cycle collector on, and
/// `fatal_error_backtraces`, php 8.5's, `On` by default
/// (`Z/Zend/zend.c:263`), which a fatal's trace reads,
/// under the CLI's hardcoded overrides (`Z/sapi/cli/php_cli.c:116-121`),
/// which is what the oracle answers with no `php.ini`. `max_execution_time`
/// and `html_errors` are the two the CLI moves, from `30` and `1`.
/// `error_prepend_string` and `error_append_string` are `NULL` in php's
/// table and empty here, since an entry with no value at all is one
/// `ini_set()` refuses to set; so are `internal_encoding`,
/// `input_encoding` and `output_encoding`, which an extension reads
/// before `default_charset` (`php_get_internal_encoding` and its kin),
/// and `default_mimetype` is `SAPI_DEFAULT_MIMETYPE`. `short_open_tag`
/// is the one entry the scanner reads, `CG(short_tags)`, and
/// `token_get_all` with it.
///
/// ```php
/// var_dump(ini_get('internal_encoding'), ini_get('output_encoding'), ini_get('default_mimetype'), ini_set('input_encoding', 'ISO-8859-1'), ini_get('input_encoding'));
/// ```
/// ```expect
/// string(0) ""
/// string(0) ""
/// string(9) "text/html"
/// string(0) ""
/// string(10) "ISO-8859-1"
/// ```
const CORE_INI: [(&str, &str); 43] = [
    ("short_open_tag", "1"),
    ("fatal_error_backtraces", "1"),
    ("precision", "14"),
    ("serialize_precision", "-1"),
    ("error_reporting", "32767"),
    ("display_errors", "1"),
    ("log_errors", "0"),
    ("html_errors", "0"),
    ("error_prepend_string", ""),
    ("error_append_string", ""),
    ("ignore_repeated_errors", "0"),
    ("ignore_repeated_source", "0"),
    ("xmlrpc_errors", "0"),
    ("xmlrpc_error_number", "0"),
    ("memory_limit", "128M"),
    ("max_execution_time", "0"),
    ("default_charset", "UTF-8"),
    ("default_mimetype", "text/html"),
    ("internal_encoding", ""),
    ("input_encoding", ""),
    ("output_encoding", ""),
    // `PHP_INCLUDE_PATH`, the pinned build's `.:${PEAR_INSTALLDIR}`,
    // which `DEFAULT_INCLUDE_PATH` says too.
    ("include_path", crate::constants::PHP_INCLUDE_PATH),
    ("zend.exception_ignore_args", "0"),
    ("zend.exception_string_param_max_len", "15"),
    ("zend.max_allowed_stack_size", "0"),
    ("zend.reserved_stack_size", "0"),
    // `zend.enable_gc`, `On` (`Z/Zend/zend.c:265`), whose `on_modify` is
    // the collector's `gc_enable`.
    ("zend.enable_gc", "1"),
    // `fiber.stack_size`, `NULL` in php's table, which `ini_get` reads
    // as the empty string and `OnUpdateFiberStackSize` as php's default.
    ("fiber.stack_size", ""),
    // `disable_functions`, `PHP_INI_SYSTEM` (`Z/main/main.c:872`): read
    // once, as the function table is built (`function_table`).
    ("disable_functions", ""),
    // `variables_order` and `request_order`, `PHP_INI_SYSTEM|PHP_INI_PERDIR`
    // (`Z/main/main.c:851`): which auto-globals the request fills and
    // the order `$_REQUEST` merges them in, read as each is made.
    ("variables_order", "EGPCS"),
    ("request_order", ""),
    // `main.c`'s entries for `mail()` and `php_syslog`, and
    // `sys_temp_dir`, `PHP_INI_SYSTEM` or `PHP_INI_SYSTEM|PHP_INI_PERDIR`
    // but `SMTP`, `smtp_port` and `syslog.filter`; a `NULL` default is
    // empty here, as the two error strings above are.
    ("sys_temp_dir", ""),
    ("SMTP", "localhost"),
    ("smtp_port", "25"),
    ("mail.add_x_header", "0"),
    ("mail.mixed_lf_and_crlf", "0"),
    ("mail.cr_lf_mode", "crlf"),
    ("mail.log", ""),
    ("sendmail_path", "/usr/sbin/sendmail -t -i"),
    ("mail.force_extra_parameters", ""),
    ("syslog.facility", "LOG_USER"),
    ("syslog.ident", "php"),
    ("syslog.filter", "no-ctrl"),
];

/// A host's ini value as the ini scanner hands it to the table
/// (`Z/Zend/zend_ini_scanner.l:491-501`): a bare `true`, `on` or `yes`
/// is `1`, a bare `false`, `off`, `no`, `none` or `null` is the empty
/// string, each in any case and with the spaces and tabs after it eaten,
/// and every other value stands as written. A `-d` goes through the same
/// scanner, `php_ini_builder_define` (`Z/main/php_ini_builder.c:63-81`)
/// writing it as a `php.ini` line, and one whose value begins with a byte
/// that is not alphanumeric it quotes first, so ` off` stands and `off `
/// does not. That is why `ini_get('fatal_error_backtraces')` under the
/// recipe's `fatal_error_backtraces=Off` answers `""` and never `Off`.
fn scanned_ini_value(value: &str) -> Vec<u8> {
    let bare = value
        .bytes()
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric());
    if !bare {
        return value.as_bytes().to_vec();
    }
    let word = value.trim_end_matches([' ', '\t']).to_ascii_lowercase();
    match word.as_str() {
        "true" | "on" | "yes" => b"1".to_vec(),
        "false" | "off" | "no" | "none" | "null" => Vec::new(),
        _ => value.as_bytes().to_vec(),
    }
}

/// `ZEND_DUMP_BEFORE_OPTIMIZER` (`Z/Zend/Optimizer/zend_optimizer.h:41`):
/// the bit of `opcache.opt_debug_level` that asks for each op array as
/// the compiler left it.
const BEFORE_OPTIMIZER: i64 = 0x1_0000;

/// `zend_ini_parse_quantity` (`Z/Zend/zend_ini.c:613-841`), which
/// `OnUpdateLong` reads every whole-number ini entry by: leading and
/// trailing whitespace ignored, an optional sign, then either a base
/// prefix — `0x`, `0o`, `0b`, or a leading `0` before a digit, which is
/// octal as `strtoul` with base zero reads it — or decimal digits, and
/// last an optional `K`, `M` or `G` multiplier with whitespace allowed
/// before it. Every shape php interprets as zero for backward
/// compatibility is zero here: no digit after the sign, no digit after
/// the prefix, an unknown prefix. What php says about those it says as an
/// `E_WARNING` from `zend_ini_parse_quantity_warn`, which no ini entry in
/// this engine raises yet; that gap is one entry's and not this
/// function's.
fn ini_quantity(value: &[u8]) -> i64 {
    let text = String::from_utf8_lossy(value);
    let text = text.trim_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (negative, rest) = match text.as_bytes().first() {
        Some(b'+') => (false, &text[1..]),
        Some(b'-') => (true, &text[1..]),
        _ => (false, text),
    };
    if !rest.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return 0;
    }
    // php reads the multiplier off the last byte of the whole value and
    // the digits off what `strtoul` stopped at, so `0k` is a multiplier
    // over no prefix and `0x10` is a prefix over no multiplier.
    let (digits, factor) = match rest.as_bytes().last().copied() {
        Some(b'g' | b'G') => (&rest[..rest.len() - 1], 1i64 << 30),
        Some(b'm' | b'M') => (&rest[..rest.len() - 1], 1i64 << 20),
        Some(b'k' | b'K') => (&rest[..rest.len() - 1], 1i64 << 10),
        _ => (rest, 1),
    };
    let digits = digits.trim_end_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (digits, radix) = match digits.as_bytes() {
        [b'0', b'x' | b'X', rest @ ..] => (rest, 16),
        [b'0', b'o' | b'O', rest @ ..] => (rest, 8),
        [b'0', b'b' | b'B', rest @ ..] => (rest, 2),
        [b'0', rest @ ..] if !rest.is_empty() => (rest, 8),
        rest => (rest, 10),
    };
    let taken: String = String::from_utf8_lossy(digits)
        .chars()
        .take_while(|c| c.is_digit(radix))
        .collect();
    // `strtoul` stopping where it started is php's "no valid leading
    // digits", which is zero; stopping at the end of what it can hold is
    // the overflow it keeps the result of.
    if taken.is_empty() {
        return 0;
    }
    let magnitude = i64::from_str_radix(&taken, radix)
        .unwrap_or(i64::MAX)
        .saturating_mul(factor);
    if negative { -magnitude } else { magnitude }
}

/// `zend_disable_functions` (`Z/Zend/zend_API.c:3668-3710`): the names
/// the startup ini's `disable_functions` takes out of the function table
/// before any request runs, so a call to one is a call to an undefined
/// function and no lookup, `function_exists()`, `is_callable()` or
/// `get_defined_functions()` finds it. The list is split at every space
/// and every comma and at nothing else, so a tab or a `;` is a byte of a
/// name. Each name is a key of the table, which holds a native under its
/// lowered name, matched byte for byte: `STRLEN` takes nothing out, and a
/// name no native has is passed over. `exit`, `die` and `clone` are the
/// three `zend_disable_function` keeps in the table; php says so with a
/// startup `E_WARNING`, `Cannot disable function %s()` in `Unknown` on
/// line 0, which this engine does not raise, since it has no diagnostic
/// before a request (#776).
fn disabled_functions(list: &[u8]) -> Vec<&[u8]> {
    list.split(|byte| matches!(byte, b' ' | b','))
        .filter(|name| !name.is_empty())
        .filter(|name| !matches!(*name, b"exit" | b"die" | b"clone"))
        .collect()
}

/// `CG(function_table)`'s natives once every `MINIT` has registered its
/// own and `php_module_startup` has called `zend_disable_functions` over
/// the startup ini's `disable_functions` (`Z/main/main.c:2370`): the engine's own
/// and then each extension's in bundle order, less what the list names
/// ([`disabled_functions`]), each beside the extension that declared it.
/// The one place the disabled set is decided: what it takes out is in no
/// table the compiler or a request reads.
fn function_table(
    extensions: &[Extension],
    ini: &[(Vec<u8>, Vec<u8>)],
) -> (Vec<Option<ExtensionId>>, Vec<(FunctionDecl, Entry)>) {
    let list = ini
        .iter()
        .find(|(name, _)| name.as_slice() == b"disable_functions")
        .map_or(&[][..], |(_, list)| list.as_slice());
    let disabled = disabled_functions(list);
    crate::natives::natives()
        .into_iter()
        .map(|native| (None, native))
        .chain(extensions.iter().zip(0u32..).flat_map(|(e, at)| {
            e.functions
                .iter()
                .cloned()
                .map(move |native| (Some(ExtensionId(at)), native))
        }))
        .filter(|(_, (decl, _))| !disabled.contains(&decl.name.0.to_ascii_lowercase().as_slice()))
        .unzip()
}

/// Builds an [`Engine`]: the bundle, the tier and the startup ini.
pub struct EngineBuilder {
    extensions: Vec<Extension>,
    ini: Vec<(Vec<u8>, Vec<u8>)>,
    entry: Loop,
    reenter: Loop,
    resume: Loop,
    tier_extension: Option<Extension>,
}

impl EngineBuilder {
    /// The extensions in `MINIT` order, the bundle's `enabled()`.
    #[must_use]
    pub fn extensions(mut self, extensions: &[Extension]) -> EngineBuilder {
        self.extensions.extend(extensions.iter().cloned());
        self
    }

    /// One ini entry as the host's `-d` or `php.ini` sets it, the value
    /// as the ini scanner hands it to the table ([`scanned_ini_value`]).
    #[must_use]
    pub fn ini(mut self, name: &str, value: &str) -> EngineBuilder {
        self.ini
            .push((name.as_bytes().to_vec(), scanned_ini_value(value)));
        self
    }

    /// Bind the tier: `execute::<T>` becomes the loop, and the tier's
    /// state is declared under seam 3 (design §4.3 seam 8).
    #[must_use]
    pub fn tier<T: Tier>(mut self) -> EngineBuilder {
        self.entry = execute::<T>;
        self.reenter = reenter::<T>;
        self.resume = resume::<T>;
        self.tier_extension = T::extension();
        self
    }

    /// The engine: every engine state built from the startup ini, in
    /// bundle order (ADR-0005), the declarations linked.
    #[must_use]
    pub fn build(self) -> Engine {
        let mut ini: Vec<(Vec<u8>, Vec<u8>)> = CORE_INI
            .iter()
            .map(|(n, v)| (n.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();
        let mut extensions = self.extensions;
        if let Some(tier) = self.tier_extension {
            extensions.push(tier);
        }
        for extension in &extensions {
            for entry in &extension.ini {
                ini.push((entry.name.0.to_vec(), entry.default.to_vec()));
            }
        }
        for (name, value) in self.ini {
            ini.retain(|(n, _)| *n != name);
            ini.push((name, value));
        }
        let startup = StartupIni {
            entries: ini.clone(),
        };
        let engine_states = extensions
            .iter()
            .filter_map(|e| e.engine_state.as_ref())
            .map(|build| build(&startup))
            .collect();
        let (function_modules, functions) = function_table(&extensions, &ini);
        let frameless = frameless_table(functions.iter().map(|(decl, _)| decl));
        let LinkedClasses {
            classes,
            handlers,
            natives,
            methods,
            trampolines,
        } = link_classes(&extensions);
        // Zend's own constants stand before the bundle's, as
        // `zend_register_standard_constants` runs before any `MINIT`.
        let constants: Vec<ConstDecl> = Shared::zend_constants()
            .into_iter()
            .chain(extensions.iter().flat_map(|e| e.constants.iter().cloned()))
            .collect();
        // `zend_register_constant` at `MINIT`: a string constant's value
        // is interned into the engine's table once, before any request,
        // so reading it later is a pointer and no allocation.
        let mut strings = Strings::new();
        let mut constant_values = Vec::new();
        for decl in &constants {
            let value = match &decl.value {
                ConstExpr::Null => Zval::null(),
                ConstExpr::Bool(b) => Zval::bool(*b),
                ConstExpr::Long(n) => Zval::long(*n),
                ConstExpr::Double(f) => Zval::double(*f),
                ConstExpr::Str(s) => match strings.intern(s) {
                    Ok(interned) => Zval::string(interned),
                    Err(_) => continue,
                },
                _ => continue,
            };
            constant_values.push((decl.name.0.clone(), value));
        }
        let sapi_names = crate::constants::host_names(&mut strings);
        Engine {
            shared: Arc::new(Shared {
                extensions,
                functions,
                function_modules,
                frameless,
                classes,
                handlers,
                natives,
                methods,
                trampolines,
                constants,
                engine_states,
                ini,
                constant_values,
                sapi_names,
                entry: self.entry,
                reenter: self.reenter,
                resume: self.resume,
                units: Cache::new(MAX_ACCELERATED_FILES),
                strings: Mutex::new(strings),
                map_ptr_last: AtomicU32::new(0),
            }),
        }
    }
}

/// The process-wide engine (design §5.14): immutable once built, shared
/// by every request thread.
#[derive(Clone)]
pub struct Engine {
    pub(crate) shared: Arc<Shared>,
}

impl Engine {
    /// A builder with no extensions, `Nop` as the tier and the core ini.
    #[must_use]
    pub fn builder() -> EngineBuilder {
        EngineBuilder {
            extensions: Vec::new(),
            ini: Vec::new(),
            entry: execute::<Nop>,
            reenter: reenter::<Nop>,
            resume: resume::<Nop>,
            tier_extension: Nop::extension(),
        }
    }

    /// `zend_flf_functions` by name, what `Unit::dump` prints a
    /// `FRAMELESS_ICALL_*` with in parentheses: the declared name of the
    /// native each row of the engine's frameless table is, in the
    /// table's order, so the `extended_value` a unit this engine compiled
    /// carries indexes the name php's dump would print.
    #[must_use]
    pub fn frameless(&self) -> Vec<Name> {
        self.shared
            .frameless
            .iter()
            .map(|entry| {
                self.shared
                    .functions
                    .get(entry.function as usize)
                    .map(|(decl, _)| decl.name.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    /// The extensions this engine loaded, `Core` first and then the
    /// bundle's in startup order, as `get_loaded_extensions()` and the
    /// CLI's `-m` list them: what a host prints without a request.
    #[must_use]
    pub fn loaded_extensions(&self) -> Vec<String> {
        std::iter::once(String::from("Core"))
            .chain(self.shared.extensions.iter().map(|e| e.name.clone()))
            .collect()
    }

    /// The compiled unit for `source`, as the cache holds it: its
    /// constant strings interned into this engine's table, so two units
    /// carrying one constant carry the one `zend_string` (design §5.2).
    #[must_use]
    pub fn unit(&self, source: &[u8], filename: &[u8]) -> Option<Arc<Unit>> {
        let stack = self.shared.stack_limit();
        let cwd = std::env::current_dir().unwrap_or_default();
        let origin = Origin {
            filename,
            cwd: &cwd,
        };
        self.shared.publish(source, origin, stack).ok()
    }

    /// What php prints on the second stream for the program
    /// `description` names before it runs it, `zend_optimize_op_array`'s
    /// `ZEND_DUMP_BEFORE_OPTIMIZER` arm
    /// (`Z/Zend/Optimizer/zend_optimizer.c:1055-1062`): the unit's op
    /// arrays through `Unit::dump`, or nothing where the engine's
    /// `opcache.opt_debug_level` carries no `0x10000` bit and nothing
    /// where the program does not compile (design §6.1).
    ///
    /// **The host asks for it, and asks once, before it serves.** php's
    /// dump is opcache's, taken as the unit is persisted, which is
    /// after the compiler and before the optimizer; this engine has no
    /// optimizer and one place a program is compiled, so what is left of
    /// php's moment is the host's, between describing the request and
    /// serving it. `php -r`'s code is the one program php dumps nothing
    /// for, `zend_optimize_op_array`'s `op_array->type ==
    /// ZEND_EVAL_CODE` return, and the host is what knows a program is
    /// that (`php_sapi::cli`).
    ///
    /// A file's unit is the cache's, the very one the request that
    /// follows serves; a source the host holds is compiled a second time
    /// here, into the engine's own table rather than the request's, since
    /// a unit compiled for one request is not reachable from outside it.
    /// The dump reads what the compiler emitted and nothing of where the
    /// strings live, so the two compiles print one dump.
    #[must_use]
    pub fn dump_before_optimizer(&self, description: &RequestDescription) -> Option<String> {
        let level = ini_quantity(self.shared.ini_default(b"opcache.opt_debug_level")?);
        if level & BEFORE_OPTIMIZER == 0 {
            return None;
        }
        let stack = self.shared.stack_limit();
        let script = description
            .script_name
            .clone()
            .unwrap_or_else(|| b"Standard input code".to_vec());
        let unit = match &description.source {
            Some(source) => {
                let origin = Origin {
                    filename: &script,
                    cwd: &description.cwd,
                };
                self.shared.publish(source, origin, stack).ok()?
            }
            None => self.shared.cached(&script, &description.cwd, stack)?.ok()?,
        };
        Some(unit.dump(&self.frameless()))
    }

    /// The one request transaction (design §5.14): compile, seed, serve,
    /// commit, shut down, observe.
    ///
    /// An engine with no extensions serves what uses Zend alone (design
    /// §4.10): the loop over the main op array, `ECHO` through the
    /// string kernel, and the engine's own natives.
    ///
    /// ```php
    /// echo 1; var_dump(extension_loaded('Core'));
    /// ```
    /// ```expect
    /// 1bool(true)
    /// ```
    ///
    /// A source php refuses ends the request the way php ends one:
    /// `E_PARSE` through `php_error_cb`, the parser's first diagnostic in
    /// php's words at php's line, and php's status. The message is the
    /// parser's, held to the oracle byte for byte; only the file name is
    /// the harness's.
    ///
    /// ```php
    /// echo 1 +;
    /// ```
    /// ```expect exit=255
    /// Parse error: syntax error, unexpected token ";" in %s on line 2
    /// ```
    ///
    /// What php's scanner raised before its parser refused is rendered
    /// before the refusal, in the order php raised them, since php's
    /// `php_error_cb` had already written each when the parser bailed
    /// out; the scanner's octal overflow is an `E_COMPILE_WARNING`
    /// (design §6.1).
    ///
    /// ```php
    /// echo "\777";
    /// echo 1 +;
    /// ```
    /// ```expect exit=255
    /// Warning: Octal escape sequence overflow \777 is greater than \377 in %s on line 2
    ///
    /// Parse error: syntax error, unexpected token ";" in %s on line 3
    /// ```
    ///
    /// The `(unset)` cast is what php's compiler refuses after the whole
    /// file has parsed, `zend_compile_cast`'s `E_COMPILE_ERROR`: the
    /// `Fatal error:` head, and php's status for a fatal, with the
    /// scanner's deprecation on the line before it rendered first. The
    /// frame carries no `Stack trace:` under the recipe, whose
    /// `fatal_error_backtraces=Off` is `run-tests.php`'s own; php's default
    /// of `On` prints `#0 {main}` under it, which
    /// `a_compile_error_carries_php_s_trace_under_the_default_ini` below
    /// holds, since no `ini_set()` runs before the compiler refuses.
    ///
    /// ```php
    /// $x = 1; (integer)$x;
    /// (unset)$x;
    /// echo 'never';
    /// ```
    /// ```expect exit=255
    /// Deprecated: Non-canonical cast (integer) is deprecated, use the (int) cast instead in %s on line 2
    ///
    /// Fatal error: The (unset) cast is no longer supported in %s on line 3
    /// ```
    pub fn serve(&self, description: RequestDescription, sink: &mut dyn OutputSink) -> Observables {
        self.transaction(description, sink, true)
    }

    /// `php_lint_script` under the CLI's `-l` (`Z/main/main.c:2779-2798`,
    /// `Z/sapi/cli/php_cli.c:955-960`): the request started, the program
    /// compiled as [`Engine::serve`] compiles it, what the compiler raised
    /// rendered where `serve` renders it, and the request shut down with
    /// no frame pushed. The exit is `serve`'s for a compile that was
    /// refused, php's status for a parse error, and 0 for one that was
    /// not, whatever the compiler warned of; the two sentences `do_cli`
    /// prints around it are the host's, read off that status.
    pub fn lint(&self, description: RequestDescription, sink: &mut dyn OutputSink) -> Observables {
        self.transaction(description, sink, false)
    }

    /// The one transaction behind [`Engine::serve`] and [`Engine::lint`]:
    /// `run` says whether the compiled unit is served on a frame or, as
    /// `php_lint_script` does, dropped once compiled.
    fn transaction(
        &self,
        description: RequestDescription,
        sink: &mut dyn OutputSink,
        run: bool,
    ) -> Observables {
        let source = description.source.clone();
        let cwd = description.cwd.clone();
        let script = description
            .script_name
            .clone()
            .unwrap_or_else(|| b"Standard input code".to_vec());
        let web = description.is_web;
        let mut req = Request::new(Arc::clone(&self.shared), description);
        // `php_request_startup` hashes the environment before the
        // extensions' `RINIT` (`Z/main/main.c:1942`); what can fail is the
        // memory limit, which the first allocation after it meets again.
        let _ = req.hash_environment();
        for extension in &self.shared.extensions {
            if let Some(rinit) = extension.rinit {
                let _ = rinit(&mut req);
            }
        }
        // What `do_cli` does once `php_request_startup` has run, before
        // the program is compiled: `$_SERVER` named, which makes it
        // (`Z/sapi/cli/php_cli.c:941`), and the standard handles
        // registered.
        if !web {
            let _ = req.arm(php_unit::AutoGlobal::Server);
        }
        let _ = req.register_file_handles();
        // Where the unit's constant strings live is where the unit
        // lives (design §5.2): a file's unit is published into the
        // engine's table and shared by every request that serves it,
        // and a source the host handed over is this request's alone,
        // interned into this request's own memory and gone with it, as
        // php holds what it compiles for `-r` and for `eval`.
        let compiled = if let Some(source) = source {
            let ini = CompileIni {
                precision: req.precision(),
                short_open_tag: req.short_open_tag(),
            };
            let shared = Arc::clone(&self.shared);
            let stack = req.stack_limit();
            Some(compile(
                &source,
                Origin {
                    filename: &script,
                    cwd: &cwd,
                },
                ini,
                shared.declared(),
                stack,
                req.heap_mut(),
                &shared.map_ptr_last,
            ))
        } else {
            req.note_primary_file(&script);
            self.shared.cached(&script, &cwd, req.stack_limit())
        };
        match compiled {
            Some(Ok(unit)) if run => serve_unit(&mut req, &unit, self.shared.entry),
            // `php_lint_script` compiles and drops the op array, and what
            // the compiler warned of it rendered as it compiled.
            Some(Ok(unit)) => note_compiled(&mut req, &unit),
            // `php_error_cb` renders it where it renders every
            // diagnostic, at the level the compiler refused at, and the
            // request ends before a frame is pushed.
            Some(Err(refused)) => {
                let file = req.current_file().map(<[u8]>::to_vec).unwrap_or_default();
                for raised in &refused.raised {
                    req.note_error_in(raised.level, &raised.message, &file, raised.line);
                }
                req.end_in_compile_time(refused.level, &refused.message, &file, refused.line);
            }
            None => {}
        }
        // `php_request_shutdown`'s order (design §5.10): the shutdown
        // functions first, which is `php_call_shutdown_functions`; then
        // the destructors of what is still live, which is
        // `zend_call_destructors`, the symbol table walked in reverse
        // and the object store walked after it; then every output
        // handler popped (`php_output_end_all`), before the extensions'
        // `rshutdown`, in reverse bundle order; the output layer
        // deactivated after them, with the header block committed if
        // nothing had, and the shutdown entries freed behind it
        // (`php_free_shutdown_functions`); then `shutdown_executor`,
        // which closes the open resources first, destroys the symbol
        // table after them (`zend_shutdown_executor_values`) and frees
        // the object storage last. The globals stand through all of it,
        // as php's `EG(symbol_table)` does, so the shutdown functions
        // and an extension's `rshutdown` read a request that still has
        // them.
        crate::natives::call_shutdown_functions(&mut req);
        req.shutdown_destructors();
        req.call_destructors();
        crate::output::end_all(&mut req);
        for extension in self.shared.extensions.iter().rev() {
            if let Some(rshutdown) = extension.rshutdown {
                rshutdown(&mut req);
            }
        }
        crate::output::deactivate(&mut req);
        req.free_shutdown_functions();
        req.close_resources();
        req.free_user_handlers();
        req.destroy_symbol_table();
        req.free_object_storage();
        // `php_error_cb` rendered the fatal where it was raised, on
        // whichever of the two streams the ini entries name; what is left
        // for the host is the log form of it, whether or not `log_errors`
        // put that form on the log.
        let fatal = match req.ending() {
            Some(Ending::Fatal(logged)) => Some(logged.clone()),
            _ => None,
        };
        let exit_code = req.exit_status();
        let headers = req.header_block();
        let body = req.take_body();
        for aside in req.take_aside() {
            match aside {
                Aside::Log(entry) => sink.log(&entry),
                Aside::Stderr(bytes) => sink.write_stderr(&bytes),
            }
        }
        sink.send_headers(&headers);
        sink.write(&body);
        sink.flush();
        Observables {
            exit_code,
            headers_sent: true,
            fatal,
        }
    }
}

/// The ini entries php reads while a file compiles, read once where the
/// compile begins and handed down: `precision`, which `zend_double_to_str`
/// renders a folded double constant at (`EG(precision)`), and
/// `short_open_tag`, which php's scanner opens a `<?` under
/// (`CG(short_tags)`, `Z/main/main.c:820`). php keeps both where its
/// compiler reads them as it compiles, so what the ini says when the
/// compile begins is what the whole file is compiled under: the startup ini's
/// for a unit bound for the cache, which every request that serves it
/// shares, and the request's own for a source that request holds.
#[derive(Debug, Clone, Copy)]
struct CompileIni {
    precision: i32,
    short_open_tag: bool,
}

/// One source compiled, with what the compiler refused kept instead of
/// dropped: what `serve` needs to render php's parse error, and what
/// [`Engine::unit`] is the lossy face over. `origin` is the name the
/// unit carries and the request's working directory, `strings` the home
/// the unit's constant strings are interned into (design §5.2), `ini`
/// the entries php's compiler reads as it compiles ([`CompileIni`]),
/// `declared` the engine's tables: the constants the bundle declared, which the
/// compiler substitutes where they are written, the natives, which it
/// reads as `CG(function_table)` before it serves a call of
/// `zend_try_compile_special_func`'s table itself, and the classes, which
/// it reads as `CG(class_table)` to know a method's callee; and `stack`
/// the calling thread's limit, which the compiler's recursion is held to;
/// and `map_ptr_last` the engine's count of map pointers, which every op
/// array of the unit takes one from ([`assign_map_ptrs`]).
fn compile(
    source: &[u8],
    origin: Origin<'_>,
    ini: CompileIni,
    declared: Declared<'_>,
    stack: StackLimit,
    strings: &mut dyn Intern,
    map_ptr_last: &AtomicU32,
) -> Result<Arc<Unit>, CompileError> {
    let options = php_compile::Options {
        filename: Name(origin.filename.into()),
        cwd: origin.cwd.as_os_str().as_encoded_bytes().to_vec(),
        strict_types: false,
        precision: ini.precision,
        short_open_tag: ini.short_open_tag,
        persistent: declared.constants.to_vec(),
        functions: declared
            .functions
            .iter()
            .map(|(decl, _)| decl.clone())
            .collect(),
        classes: declared.classes.to_vec(),
        stack,
    };
    let mut unit = php_compile::compile_source(source, &options, strings)?;
    assign_map_ptrs(&mut unit, map_ptr_last);
    Ok(Arc::new(unit))
}

/// What compiling the unit raised, raised now, each through the
/// path a runtime diagnostic takes and at the unit's own file and the
/// compiler's line, which is `zend_emit_recorded_errors_ex` over a
/// cached `persistent_script`'s `warnings`
/// (`Z/ext/opcache/ZendAccelerator.c:1939`) and, for a unit compiled by
/// this request, where `zend_error` under `CG(in_compilation)` had
/// already put it. So the order against what the program writes is
/// php's, and a cache hit raises what a miss raised, as the oracle does
/// with no cache at all.
///
/// ```php
/// echo 'a';
/// echo NAN . 'z';
/// ```
/// ```expect
/// Warning: unexpected NAN value was coerced to string in %s on line 3
/// aNANz
/// ```
///
/// The scanner's are among them, before the compiler's, at the levels
/// php raises each under (design §6.1): the octal escape overflow is an
/// `E_COMPILE_WARNING` and the byte written is the low eight bits of
/// what the digits spell, `(char) ZEND_STRTOL`; a non-canonical cast
/// spelling is the `E_DEPRECATED` 8.5 raises, and the cast itself runs.
///
/// ```php
/// echo "\777", "|", ord("\777"), "|", "\101\x41";
/// ```
/// ```expect
/// Warning: Octal escape sequence overflow \777 is greater than \377 in %s on line 2
///
/// Warning: Octal escape sequence overflow \777 is greater than \377 in %s on line 2
/// %c|255|AA
/// ```
///
/// ```php
/// $x = "7 apples"; echo (integer)$x, "|", (double)$x, "|", (binary)$x, "|", (boolean)$x;
/// ```
/// ```expect
/// Deprecated: Non-canonical cast (integer) is deprecated, use the (int) cast instead in %s on line 2
///
/// Deprecated: Non-canonical cast (double) is deprecated, use the (float) cast instead in %s on line 2
///
/// Deprecated: Non-canonical cast (binary) is deprecated, use the (string) cast instead in %s on line 2
///
/// Deprecated: Non-canonical cast (boolean) is deprecated, use the (bool) cast instead in %s on line 2
/// 7|7|7 apples|1
/// ```
///
/// Before them, each auto-global the unit names is armed
/// ([`Unit::arms`]), as php's compiler arms one where it meets the name
/// and opcache arms a cached file's `ping_auto_globals_mask` on a hit
/// (`Z/ext/opcache/ZendAccelerator.c:1941`), so one made just in time is
/// in `EG(symbol_table)` before the unit's first line, where the file
/// names it and whether or not the line that names it is reached.
pub(crate) fn note_compiled(req: &mut Request, unit: &Arc<Unit>) {
    for global in &unit.arms {
        // What a callback can raise is the memory limit, which the
        // loop meets again at its first allocation.
        let _ = req.arm(*global);
    }
    for raised in &unit.diagnostics {
        req.note_error_in(
            raised.level,
            &raised.message,
            &unit.main.filename.0,
            raised.line,
        );
    }
}

/// The unit's main op array served on a fresh entry frame, what it
/// compiled to raised first, the frame attached to `EG(symbol_table)`,
/// which is how its `$argv` and `$argc` are the ones
/// `php_hash_environment` wrote there; the loop's fatal, if any, ends
/// the request.
///
/// The frame is left the way php leaves the main one, which is not the
/// way it leaves any other: its CVs are `EG(symbol_table)`, so
/// `zend_detach_symbol_table` moves them to the request before the pop
/// ([`Request::detach_symbol_table`]) and the pop frees a frame whose
/// CVs are all `UNDEF`. The variables are then the request's, through
/// the shutdown functions and the extensions' `rshutdown`, until
/// `shutdown_executor` destroys the table (design §5.10).
fn serve_unit(req: &mut Request, unit: &Arc<Unit>, entry: Loop) {
    note_compiled(req, unit);
    if !req.declare_classes(unit) || !req.declare_functions(unit) {
        return;
    }
    let frame = match req.push_frame(Arc::clone(unit)) {
        Ok(frame) => frame,
        Err(fatal) => {
            req.end_in(&fatal);
            return;
        }
    };
    if let Err(fatal) = entry(req, frame) {
        req.end_in(&fatal);
    }
    req.detach_symbol_table(frame);
    req.pop_frame();
    // What the loop left pending for the user exception handler, served
    // with the main frame left (`zend_execute_scripts`).
    if req.is_pending()
        && let Err(fatal) = req.uncaught()
    {
        req.end_in(&fatal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Asked, Probe};
    use crate::tier::Site;

    /// One call the sink saw on a channel beside the body.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Saw {
        Log(Vec<u8>),
        Stderr(Vec<u8>),
    }

    /// Everything the request wrote, kept: the body as `Capture` keeps
    /// it behind the `test` feature, and the log and the error stream
    /// as the calls the sink saw, in the order it saw them.
    #[derive(Default)]
    struct Kept {
        body: Vec<u8>,
        seen: Vec<Saw>,
    }

    impl Kept {
        /// The log's entries alone, one per call.
        fn log(&self) -> Vec<Vec<u8>> {
            self.seen
                .iter()
                .filter_map(|saw| match saw {
                    Saw::Log(entry) => Some(entry.clone()),
                    Saw::Stderr(_) => None,
                })
                .collect()
        }

        /// The log as php's CLI writes it to stderr, `"%s\n"` per entry.
        fn logged(&self) -> String {
            let mut text = String::new();
            for entry in self.log() {
                text.push_str(&String::from_utf8_lossy(&entry));
                text.push('\n');
            }
            text
        }

        /// The error stream alone, as a host with an `error_log` file
        /// would have it, the log parted from it.
        fn stderr(&self) -> String {
            let mut text = String::new();
            for saw in &self.seen {
                if let Saw::Stderr(bytes) = saw {
                    text.push_str(&String::from_utf8_lossy(bytes));
                }
            }
            text
        }
    }

    impl OutputSink for Kept {
        fn write(&mut self, bytes: &[u8]) {
            self.body.extend_from_slice(bytes);
        }
        fn flush(&mut self) {}
        fn send_headers(&mut self, _: &[u8]) {}
        fn log(&mut self, entry: &[u8]) {
            self.seen.push(Saw::Log(entry.to_vec()));
        }
        fn write_stderr(&mut self, bytes: &[u8]) {
            self.seen.push(Saw::Stderr(bytes.to_vec()));
        }
    }

    fn description(source: &str) -> RequestDescription {
        RequestDescription {
            sapi_name: String::from("cli"),
            script_name: Some(b"witness.php".to_vec()),
            source: Some(source.as_bytes().to_vec()),
            ..RequestDescription::default()
        }
    }

    /// `sapi_send_headers` under a host whose `header_handler` takes the
    /// line: the default `Content-type` stands in the block once the
    /// first body byte reaches it; under the CLI's description the block
    /// carries nothing, since `sapi_cli_header_handler` answers zero and
    /// the line is freed (#289).
    #[test]
    fn the_default_content_type_reaches_a_web_host_s_block_and_not_the_cli_s() {
        let engine = Engine::builder().build();
        let mut web = crate::test::Capture::default();
        engine.serve(
            RequestDescription {
                is_web: true,
                sapi_name: String::from("cgi-fcgi"),
                ..description("<?php echo 'x';")
            },
            &mut web,
        );
        assert_eq!(web.stdout, b"x");
        assert_eq!(web.headers, b"Content-type: text/html; charset=UTF-8\r\n");
        let mut cli = crate::test::Capture::default();
        engine.serve(description("<?php echo 'x';"), &mut cli);
        assert_eq!(cli.stdout, b"x");
        assert_eq!(cli.headers, b"");
    }

    /// `__DIR__` where the request's name has no directory is the
    /// request's working directory, `VCWD_GETCWD` (#366): two requests
    /// in one process described with two directories read two, which is
    /// php's under ZTS and what design §3's threads need; the process's
    /// own directory is neither.
    #[test]
    fn dir_is_the_described_request_s_working_directory_not_the_process_s() {
        let engine = Engine::builder().build();
        let dir_under = |cwd: &str| {
            let mut sink = Kept::default();
            engine.serve(
                RequestDescription {
                    cwd: PathBuf::from(cwd),
                    ..description("<?php echo __DIR__;")
                },
                &mut sink,
            );
            String::from_utf8_lossy(&sink.body).into_owned()
        };
        assert_eq!(dir_under("/srv/one"), "/srv/one");
        assert_eq!(dir_under("/srv/two"), "/srv/two");
    }

    fn served(engine: &Engine, source: &str) -> (String, Observables) {
        let mut sink = Kept::default();
        let observables = engine.serve(description(source), &mut sink);
        (
            String::from_utf8_lossy(&sink.body).into_owned(),
            observables,
        )
    }

    /// What the pinned oracle answers for
    /// `var_dump(ini_get('error_append_string'))` under
    /// `-d error_append_string=<value>`: `Off`, `OFF`, `oFf`, `FALSE`,
    /// `none`, `None`, `NULL` and `off ` are `string(0) ""`; `On`, `YES`
    /// and `true` are `string(1) "1"`; `0`, `2`, `abc`, `stderr`, `1M`,
    /// `of` and ` off` stand as written.
    #[test]
    fn a_host_s_ini_value_is_read_as_the_scanner_reads_a_bare_word() {
        let read = |value: &str| {
            let engine = Engine::builder().ini("error_append_string", value).build();
            engine
                .shared
                .ini_default(b"error_append_string")
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        };
        for off in ["Off", "OFF", "oFf", "FALSE", "none", "None", "NULL", "off "] {
            assert_eq!(read(off).as_deref(), Some(""), "{off:?}");
        }
        for on in ["On", "YES", "true"] {
            assert_eq!(read(on).as_deref(), Some("1"), "{on:?}");
        }
        for written in ["0", "2", "abc", "stderr", "1M", "of", " off", ""] {
            assert_eq!(read(written).as_deref(), Some(written), "{written:?}");
        }
    }

    /// What the pinned oracle answers under `-n -d disable_functions=<list>`
    /// for [`disabled_functions`]'s cases: a disabled native is no
    /// function to a call, a dynamic call, `function_exists()`,
    /// `get_defined_functions()` or a callback's check; the list splits
    /// at spaces and commas alone; a name is matched as the table's
    /// lowered key; an unknown one is passed over; `exit` and `die` stay;
    /// and a user function may take a disabled native's name (Zend's
    /// `bug79382.phpt`).
    #[test]
    fn disable_functions_takes_its_natives_out_of_the_function_table() {
        const PROBE: &str = r#"<?php
            $listed = false;
            foreach (get_defined_functions()['internal'] as $name) { $listed = $listed || $name === 'strlen'; }
            var_dump(function_exists('strlen'), function_exists('defined'), function_exists('exit'), $listed);
            try { strlen('a'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
            try { $f = 'defined'; $f('A'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
            try { set_error_handler('strlen'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
        "#;
        const UNDEFINED_STRLEN: &str = "Call to undefined function strlen()\n";
        const UNDEFINED_DEFINED: &str = "Call to undefined function defined()\n";
        const CALLBACK: &str = "set_error_handler(): Argument #1 ($callback) must be a valid callback or null, function \"strlen\" not found or invalid function name\n";
        let served_under = |list: &str| {
            let engine = Engine::builder().ini("disable_functions", list).build();
            served(&engine, PROBE).0
        };
        let answers = |strlen: bool, defined: bool| {
            let mut expected =
                format!("bool({strlen})\nbool({defined})\nbool(true)\nbool({strlen})\n");
            if !strlen {
                expected.push_str(UNDEFINED_STRLEN);
            }
            if !defined {
                expected.push_str(UNDEFINED_DEFINED);
            }
            if !strlen {
                expected.push_str(CALLBACK);
            }
            expected
        };
        for (list, strlen, defined) in [
            ("", true, true),
            ("strlen", false, true),
            (" strlen , defined", false, false),
            ("strlen,,defined", false, false),
            ("STRLEN", true, true),
            ("nosuchfn,strlen", false, true),
            ("\tstrlen", true, true),
            ("die,strlen", false, true),
            ("exit", true, true),
        ] {
            assert_eq!(served_under(list), answers(strlen, defined), "{list:?}");
        }
        let engine = Engine::builder().ini("disable_functions", "strlen").build();
        let (out, _) = served(
            &engine,
            "<?php function strlen($x) { return 42; } var_dump(strlen('foobar'));",
        );
        assert_eq!(out, "int(42)\n");
    }

    /// `fatal_error_backtraces` is `On` in php's table (`Z/Zend/zend.c:263`)
    /// and the CLI leaves it, so `php -n` over
    /// `<?php\nvar_dump(ini_get('fatal_error_backtraces'));` prints
    /// `string(1) "1"`; the recipe's `Off` is `run-tests.php`'s, which is
    /// why the witness beside `Request::fatal_error_backtraces` reads `""`.
    #[test]
    fn fatal_error_backtraces_is_on_by_default() {
        let engine = Engine::builder().build();
        let (body, _) = served(
            &engine,
            "<?php\nvar_dump(ini_get('fatal_error_backtraces'));",
        );
        assert_eq!(body, "string(1) \"1\"\n");
    }

    /// The compiler's `E_COMPILE_ERROR` goes through `zend_error_impl`, so
    /// under php's default it fetches its trace, `#0 {main}` with no
    /// frame standing, and the parser's `E_PARSE` is a `ParseError`
    /// `zend_exception_error` renders with none. What the pinned oracle
    /// writes under `php -n` for `<?php\n(unset)$x;` is the fatal, then
    /// `Stack trace:` and `#0 {main}`, in the plain form and, under
    /// `-d html_errors=1`, in the second html format, the trace between
    /// `<br />` and the newline; and for `<?php\necho 1 +;` the parse
    /// error alone, under both.
    #[test]
    fn a_compile_error_carries_php_s_trace_under_the_default_ini() {
        let engine = Engine::builder().build();
        let (body, observables) = served(&engine, "<?php\n(unset)$x;");
        assert_eq!(
            body,
            "\nFatal error: The (unset) cast is no longer supported in witness.php on line 2\nStack trace:\n#0 {main}\n"
        );
        assert_eq!(
            observables.fatal.as_deref(),
            Some(
                "PHP Fatal error:  The (unset) cast is no longer supported in witness.php on line 2\nStack trace:\n#0 {main}"
            )
        );
        let (body, observables) = served(&engine, "<?php\necho 1 +;");
        assert_eq!(
            (body.as_str(), observables.exit_code),
            (
                "\nParse error: syntax error, unexpected token \";\" in witness.php on line 2\n",
                255
            )
        );

        let html = Engine::builder().ini("html_errors", "1").build();
        let (body, _) = served(&html, "<?php\n(unset)$x;");
        assert_eq!(
            body,
            "<br />\n<b>Fatal error</b>:  The (unset) cast is no longer supported in <b>witness.php</b> on line <b>2</b><br />\nStack trace:\n#0 {main}\n"
        );
        let (body, _) = served(&html, "<?php\necho 1 +;");
        assert_eq!(
            body,
            "<br />\n<b>Parse error</b>:  syntax error, unexpected token &quot;;&quot; in <b>witness.php</b> on line <b>2</b><br />\n"
        );
    }

    /// A class the compiler bound with no opline (`zend_compile_class_decl`
    /// under `toplevel`) is declared into the request's table before the
    /// main op array runs, so the source the doc-comment witness of
    /// `php_syntax` declares runs (#257); a name Zend registered is
    /// `zend_class_redeclaration_error` in the wording for an internal
    /// class, and one a unit of this request declared names its site.
    /// Both are `E_COMPILE_ERROR` at the class's line, rendered before
    /// any of the file runs, over `#0 {main}` under php's default
    /// `fatal_error_backtraces`, which is what the pinned oracle writes
    /// for `<?php\necho 1;\nclass Exception {}` under `php -n`.
    #[test]
    fn a_unit_s_classes_are_declared_before_its_main_op_array_runs() {
        let engine = Engine::builder().build();
        let (body, observables) = served(
            &engine,
            "<?php\nclass M { /** P */ public $x, $y; /** K */ const A = 1, B = 2; }\necho 1;",
        );
        assert_eq!((body.as_str(), observables.exit_code), ("1", 0));
        let (body, observables) = served(&engine, "<?php\necho 1;\nclass Exception {}");
        assert_eq!(
            (body.as_str(), observables.exit_code),
            (
                "\nFatal error: Cannot redeclare class Exception in witness.php on line 3\nStack trace:\n#0 {main}\n",
                255
            )
        );
        let (body, observables) = served(&engine, "<?php\necho 1;\nclass M {}\nclass m {}");
        assert_eq!(
            (body.as_str(), observables.exit_code),
            (
                "\nParse error: a class declared twice this compiler cannot read yet in witness.php on line 4\n",
                255
            ),
            "php emits a DECLARE_CLASS for the second and fails it at run time; that opline is not emitted yet"
        );
    }

    #[test]
    fn an_engine_with_no_extensions_serves_what_uses_zend_alone() {
        let engine = Engine::builder().build();
        let (body, observables) = served(&engine, "<?php echo 1;");
        assert_eq!(body, "1");
        assert_eq!(observables.exit_code, 0);
        assert_eq!(observables.fatal, None);
    }

    /// A construct php accepts and this compiler cannot read yet is
    /// refused in the compiler's own sentence, which says which construct
    /// it could not read; a construct php refuses is php's `syntax
    /// error`, and the witness beside `serve` holds that to the oracle.
    /// The frame around either is php's.
    #[test]
    fn a_source_the_compiler_refuses_names_the_construct_it_could_not_read() {
        let engine = Engine::builder().build();
        let (body, observables) = served(&engine, "<?php\ndeclare(ticks=1);");
        assert_eq!(
            body,
            "\nParse error: a `declare(ticks)` of a count this compiler cannot read yet in witness.php on line 2\n"
        );
        assert_eq!(observables.exit_code, 255);
        assert_eq!(
            observables.fatal, None,
            "a parse error is not the fatal `serve` renders a second time"
        );
    }

    /// A parse error is `error_reporting`'s and `log_errors`' to render,
    /// as every other diagnostic is; the status is php's either way.
    #[test]
    fn a_parse_error_is_logged_and_masked_the_way_php_masks_one() {
        let mut logged = Kept::default();
        let observables = Engine::builder()
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\ndeclare(ticks=1);"), &mut logged);
        assert!(logged.body.is_empty());
        assert_eq!(
            logged.logged(),
            "PHP Parse error:  a `declare(ticks)` of a count this compiler cannot read yet in witness.php on line 2\n"
        );
        assert_eq!(observables.exit_code, 255);

        let mut masked = Kept::default();
        let observables = Engine::builder()
            .ini("error_reporting", "0")
            .build()
            .serve(description("<?php\ndeclare(ticks=1);"), &mut masked);
        assert!(masked.body.is_empty(), "E_PARSE is masked out");
        assert_eq!(observables.exit_code, 255, "the status is php's regardless");
    }

    /// What php's compiler refuses after parsing is `E_COMPILE_ERROR`,
    /// rendered as a fatal and ending the request as one, with the log
    /// form beside it: the compiler's stack limit (design §5.6), which
    /// `zend.max_allowed_stack_size` bounds from where the request
    /// started, on the thread the request runs on. A chain of 200,000
    /// links is a tree that deep, and the frame that trips the limit
    /// is refused in php's sentence, with the number php prints, and
    /// nothing of the tree left to a native overflow, on the way down
    /// or on the way out. The witness beside `php_compile::StackLimit`
    /// holds the sentence to the oracle; what is held here is the
    /// frame the engine puts around it, on a thread of the test
    /// harness's own size.
    #[test]
    fn what_the_compiler_refuses_past_its_stack_limit_is_a_fatal() {
        let source = format!("<?php\n$y = 1{};\necho 'never';", "+1".repeat(200_000));
        let engine = Engine::builder()
            .ini("zend.max_allowed_stack_size", "512K")
            .build();
        let (body, observables) = served(&engine, &source);
        // 512K less the least `zend.reserved_stack_size`: 491520 is the
        // oracle's number under `php -n` on macOS, where php reserves
        // 32K, and 475136 is php's where `ZEND_ALLOCA_MAX_SIZE` is
        // compiled and it reserves 48K (`php_compile::stack::RESERVED_MIN`).
        let number = if cfg!(target_vendor = "apple") {
            491_520
        } else {
            475_136
        };
        let sentence = format!(
            "Maximum call stack size of {number} bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached during compilation. Try splitting expression in witness.php on line 2\nStack trace:\n#0 {{main}}"
        );
        assert_eq!(body, format!("\nFatal error: {sentence}\n"));
        assert_eq!(observables.exit_code, 255);
        assert_eq!(
            observables.fatal.as_deref(),
            Some(format!("PHP Fatal error:  {sentence}").as_str()),
            "the log form the host is handed, as for every fatal, the trace with it"
        );

        let mut masked = Kept::default();
        let observables = Engine::builder()
            .ini("zend.max_allowed_stack_size", "512K")
            .ini("error_reporting", "0")
            .build()
            .serve(description(&source), &mut masked);
        assert!(masked.body.is_empty(), "E_COMPILE_ERROR is masked out");
        assert_eq!(observables.exit_code, 255, "the status is php's regardless");

        let (body, observables) = served(
            &Engine::builder()
                .ini("zend.max_allowed_stack_size", "-1")
                .build(),
            "<?php echo 1;",
        );
        assert_eq!(
            (body.as_str(), observables.exit_code),
            ("1", 0),
            "-1 checks nothing"
        );
    }

    /// `php_error_cb`'s two renderings of one diagnostic, which the two
    /// ini entries decide, each naming the line the opline stands on.
    /// The oracle writes the same bytes for `<?php echo NAN;` at
    /// `display_errors=1, log_errors=0` and at the two swapped.
    #[test]
    fn a_diagnostic_is_displayed_or_logged_as_the_ini_entries_say() {
        let mut displayed = Kept::default();
        Engine::builder()
            .build()
            .serve(description("<?php echo NAN;"), &mut displayed);
        assert_eq!(
            String::from_utf8_lossy(&displayed.body),
            "\nWarning: unexpected NAN value was coerced to string in witness.php on line 1\nNAN"
        );
        assert!(
            displayed.log().is_empty(),
            "log_errors is off, so nothing is logged"
        );

        let mut logged = Kept::default();
        Engine::builder()
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php echo NAN;"), &mut logged);
        assert_eq!(String::from_utf8_lossy(&logged.body), "NAN");
        assert_eq!(
            logged.logged(),
            "PHP Warning:  unexpected NAN value was coerced to string in witness.php on line 1\n"
        );
    }

    /// The warning php writes for `echo NAN;` on line 2 of the witness
    /// regime's file, which the display-branch tests below stand on.
    const NAN_WARNING: &str = "unexpected NAN value was coerced to string";

    /// `error_prepend_string` and `error_append_string` stand around
    /// php's plain display form and around nothing else: the log form
    /// carries neither. What the pinned oracle writes for
    /// `-d error_prepend_string='<PRE>' -d error_append_string='</PRE>'`
    /// over `<?php\necho NAN;` is
    /// `<PRE>\nWarning: … on line 2\n</PRE>NAN`.
    #[test]
    fn the_two_error_strings_stand_around_the_display_form() {
        let mut around = Kept::default();
        Engine::builder()
            .ini("error_prepend_string", "<PRE>")
            .ini("error_append_string", "</PRE>")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\necho NAN;"), &mut around);
        assert_eq!(
            String::from_utf8_lossy(&around.body),
            format!("<PRE>\nWarning: {NAN_WARNING} in witness.php on line 2\n</PRE>NAN")
        );
        assert_eq!(
            around.logged(),
            format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2\n"),
            "php's log form is the one rendering neither string reaches"
        );
    }

    /// `html_errors` is php's other display form, and the two strings
    /// stand around it as they stand around the plain one. What the
    /// pinned oracle writes for `-d html_errors=1` over
    /// `<?php\necho NAN;` is
    /// `<br />\n<b>Warning</b>:  … in <b>%s</b> on line <b>2</b><br />\n`.
    #[test]
    fn html_errors_is_phps_other_display_form() {
        let mut html = Kept::default();
        Engine::builder()
            .ini("html_errors", "1")
            .build()
            .serve(description("<?php\necho NAN;"), &mut html);
        assert_eq!(
            String::from_utf8_lossy(&html.body),
            format!(
                "<br />\n<b>Warning</b>:  {NAN_WARNING} in <b>witness.php</b> on line <b>2</b><br />\nNAN"
            )
        );

        let mut around = Kept::default();
        Engine::builder()
            .ini("html_errors", "1")
            .ini("error_prepend_string", "<PRE>")
            .ini("error_append_string", "</PRE>")
            .build()
            .serve(description("<?php\necho NAN;"), &mut around);
        assert_eq!(
            String::from_utf8_lossy(&around.body),
            format!(
                "<PRE><br />\n<b>Warning</b>:  {NAN_WARNING} in <b>witness.php</b> on line <b>2</b><br />\n</PRE>NAN"
            )
        );
    }

    /// `ignore_repeated_errors` drops a repeat of the last diagnostic,
    /// and `ignore_repeated_source` says whether a repeat at another
    /// line is one. What the pinned oracle writes for
    /// `<?php\necho NAN, NAN;\necho NAN;` is three warnings with both
    /// entries off, two with `ignore_repeated_errors=1` — the second on
    /// line 2 dropped, the one on line 3 kept — and one with
    /// `ignore_repeated_source=1` beside it.
    #[test]
    fn a_repeat_is_dropped_the_way_php_drops_one() {
        let source = "<?php\necho NAN, NAN;\necho NAN;";
        let warning =
            |line: u32| format!("\nWarning: {NAN_WARNING} in witness.php on line {line}\n");

        let mut every = Kept::default();
        Engine::builder()
            .build()
            .serve(description(source), &mut every);
        assert_eq!(
            String::from_utf8_lossy(&every.body),
            format!("{}NAN{}NAN{}NAN", warning(2), warning(2), warning(3))
        );

        let mut once_a_site = Kept::default();
        Engine::builder()
            .ini("ignore_repeated_errors", "1")
            .build()
            .serve(description(source), &mut once_a_site);
        assert_eq!(
            String::from_utf8_lossy(&once_a_site.body),
            format!("{}NANNAN{}NAN", warning(2), warning(3)),
            "the repeat on line 2 is dropped and the one on line 3 stands"
        );

        let mut once = Kept::default();
        Engine::builder()
            .ini("ignore_repeated_errors", "1")
            .ini("ignore_repeated_source", "1")
            .build()
            .serve(description(source), &mut once);
        assert_eq!(
            String::from_utf8_lossy(&once.body),
            format!("{}NANNANNAN", warning(2)),
            "the site no longer parts them, so one warning stands for all three"
        );
    }

    /// `xmlrpc_errors` is php's first display form, ahead of both the
    /// plain and the html one, and the two strings stand around neither
    /// it nor the log form. What the pinned oracle writes for
    /// `-d xmlrpc_errors=1 -d xmlrpc_error_number=42
    /// -d error_prepend_string='<PRE>'` over `<?php\necho NAN;` is one
    /// `methodResponse` fault, the code `42`, the message after
    /// `Warning:` with no space, and `NAN` straight after it.
    #[test]
    fn xmlrpc_errors_is_phps_first_display_form() {
        let mut fault = Kept::default();
        Engine::builder()
            .ini("xmlrpc_errors", "1")
            .ini("xmlrpc_error_number", "42")
            .ini("error_prepend_string", "<PRE>")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\necho NAN;"), &mut fault);
        assert_eq!(
            String::from_utf8_lossy(&fault.body),
            format!(
                "<?xml version=\"1.0\"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>42</int></value></member><member><name>faultString</name><value><string>Warning:{NAN_WARNING} in witness.php on line 2</string></value></member></struct></value></fault></methodResponse>NAN"
            )
        );
        assert_eq!(
            fault.logged(),
            format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2\n"),
            "the log form is the same whichever display form is on"
        );
    }

    /// `display_errors=stderr` is the error stream under the hosts php
    /// names at `Z/main/main.c:1484`, `cli`, `cgi` and `phpdbg`, and
    /// the plain form on the body under every other. What the pinned
    /// oracle writes for `-d display_errors=stderr` over `<?php\necho
    /// NAN;` is `NAN` on stdout and `Warning: … on line 2` on stderr,
    /// with no leading newline; under `fpm-fcgi` php's test on
    /// `sapi_module.name` fails and the `else` is the plain form.
    #[test]
    fn stderr_is_the_error_stream_only_under_the_hosts_php_names() {
        let engine = Engine::builder().ini("display_errors", "stderr").build();
        let under = |host: &str| {
            let mut kept = Kept::default();
            engine.serve(
                RequestDescription {
                    sapi_name: String::from(host),
                    ..description("<?php\necho NAN;")
                },
                &mut kept,
            );
            (
                String::from_utf8_lossy(&kept.body).into_owned(),
                kept.stderr(),
            )
        };
        for host in ["cli", "cgi", "phpdbg"] {
            assert_eq!(
                under(host),
                (
                    String::from("NAN"),
                    format!("Warning: {NAN_WARNING} in witness.php on line 2\n")
                ),
                "{host}"
            );
        }
        assert_eq!(
            under("fpm-fcgi"),
            (
                format!("\nWarning: {NAN_WARNING} in witness.php on line 2\nNAN"),
                String::new()
            ),
            "another host falls to the plain form on the body"
        );
    }

    /// `display_errors` is read as `php_get_display_errors_mode`
    /// (`Z/main/main.c:492-517`) reads it: a number is `ZEND_ATOL` of the
    /// value narrowed to php's `uint8_t`, where `2` is the error stream,
    /// `0` is off and any other is the body, php's `STDOUT` fallback.
    /// Measured under the pin, `-d display_errors=2` over `<?php\necho
    /// NAN;` writes `NAN` alone to stdout and the warning to stderr, as
    /// ` 2` and `2x` do, `atoll` skipping the space and stopping at the
    /// `x`; `7` and `-1` write the plain form on the body; `256` narrows
    /// to `0` and writes `NAN` alone, and `258` narrows to `2`; a word
    /// that is none of php's five, `abc`, is `ZEND_ATOL`'s zero.
    #[test]
    fn a_numeric_display_errors_is_read_as_phps_uint8_mode() {
        let under = |value: &str| {
            let engine = Engine::builder().ini("display_errors", value).build();
            let mut kept = Kept::default();
            engine.serve(description("<?php\necho NAN;"), &mut kept);
            (
                String::from_utf8_lossy(&kept.body).into_owned(),
                kept.stderr(),
            )
        };
        let on_stderr = (
            String::from("NAN"),
            format!("Warning: {NAN_WARNING} in witness.php on line 2\n"),
        );
        let on_body = (
            format!("\nWarning: {NAN_WARNING} in witness.php on line 2\nNAN"),
            String::new(),
        );
        let off = (String::from("NAN"), String::new());
        for value in ["2", " 2", "2x", "258", "-254"] {
            assert_eq!(under(value), on_stderr, "{value:?} is the error stream");
        }
        for value in ["7", "-1", "1", "stdout", "on", "yes", "true"] {
            assert_eq!(under(value), on_body, "{value:?} is the body");
        }
        for value in ["0", "256", "abc", "", "off", "no", "false"] {
            assert_eq!(under(value), off, "{value:?} is off");
        }
    }

    /// The error stream and the log are two channels on the sink, so a
    /// host with an `error_log` file keeps the stderr form of a
    /// diagnostic off the file, and one diagnostic at
    /// `display_errors=stderr` and `log_errors=1` is two calls in
    /// php's order: `php_error_cb` logs before it displays
    /// (`Z/main/main.c:1459`, then `:1484`), and the stderr form ends in
    /// `\n` where the log entry ends in nothing. Under the CLI the two
    /// are one fd, and what it reads is
    /// `PHP Warning:  … line 2\nWarning: … line 2\n`, which is what the
    /// pinned oracle writes for `-d display_errors=stderr -d
    /// log_errors=1` over `<?php\necho NAN;`.
    #[test]
    fn the_stderr_form_and_the_log_entry_are_two_calls_in_phps_order() {
        let mut kept = Kept::default();
        Engine::builder()
            .ini("display_errors", "stderr")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\necho NAN;"), &mut kept);
        assert_eq!(String::from_utf8_lossy(&kept.body), "NAN");
        assert_eq!(
            kept.seen,
            vec![
                Saw::Log(
                    format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2").into_bytes()
                ),
                Saw::Stderr(
                    format!("Warning: {NAN_WARNING} in witness.php on line 2\n").into_bytes()
                ),
            ]
        );
    }

    /// A dropped repeat is dropped whole, php's `if (display)` around
    /// the store as well as the two renderings: it is not logged, and
    /// the diagnostic it repeats stays the one `error_get_last()`
    /// answers.
    #[test]
    fn a_dropped_repeat_is_neither_logged_nor_stored() {
        let mut logged = Kept::default();
        Engine::builder()
            .ini("ignore_repeated_errors", "1")
            .ini("ignore_repeated_source", "1")
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\necho NAN, NAN;"), &mut logged);
        assert_eq!(
            logged.logged(),
            format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2\n"),
            "one log line for the two warnings"
        );
    }

    /// The same request through the `Capture` sink, which is the shape
    /// every extension's own test reads (design §4.3 seam 4, §4.10).
    #[test]
    fn the_capture_sink_holds_what_a_zend_only_request_wrote() {
        let served = Engine::builder()
            .build()
            .serve_capture("<?php echo 1;", crate::test::Capture::default());
        assert_eq!(served.stdout, b"1");
        assert_eq!(served.exit_code, 0);
        assert_eq!(served.fatal, None);
        let served = Engine::builder().build().serve_capture(
            "<?php var_dump(extension_loaded('Core'));",
            crate::test::Capture::default(),
        );
        assert_eq!(served.stdout, b"bool(true)\n");
    }

    /// How many strings the engine has published past the ones its build
    /// interned for the constants it declares, which is what says a
    /// constant of a unit was interned once.
    fn interned(engine: &Engine) -> usize {
        let at_build = strings_of(&Engine::builder().build());
        strings_of(engine).saturating_sub(at_build)
    }

    /// How many strings the engine's table holds.
    fn strings_of(engine: &Engine) -> usize {
        match engine.shared.strings.lock() {
            Ok(strings) => strings.count(),
            Err(poisoned) => poisoned.into_inner().count(),
        }
    }

    /// `Engine::builder().build()` interns a string constant's value into
    /// its table at build, as php's `MINIT` does, and no request grows
    /// the table for reading one: every value of Core's longer than one
    /// byte, each once however many constants share it, and the hosts'
    /// names `PHP_SAPI` is read from, `PHP_EOL` being the frozen one-byte
    /// string.
    #[test]
    fn a_string_constant_is_interned_into_the_engine_at_build() {
        let engine = Engine::builder().build();
        let distinct: std::collections::BTreeSet<&[u8]> = engine
            .shared
            .constants
            .iter()
            .filter_map(|decl| match &decl.value {
                ConstExpr::Str(s) if s.len() > 1 => Some(&**s),
                _ => None,
            })
            .chain(crate::constants::HOSTS.iter().map(|name| name.as_bytes()))
            .collect();
        assert_eq!(
            strings_of(&engine),
            distinct.len(),
            "Core's strings and the hosts' names, each once"
        );
        let version = engine.shared.constant(b"PHP_VERSION");
        assert_eq!(
            version.as_ref().and_then(Value::as_str_bytes),
            Some(crate::api::request::PHP_VERSION.as_bytes())
        );
        assert_eq!(
            engine
                .shared
                .constant(b"PHP_EOL")
                .as_ref()
                .and_then(Value::as_str_bytes),
            Some(&b"\n"[..])
        );
        assert_eq!(interned(&engine), 0);
    }

    /// A unit published into the cache interns its constant strings once,
    /// and every request serving it reads the one `zend_string` (design
    /// §5.2, §5.14, §6.3): two requests over one file write the same
    /// bytes out of one allocation the engine owns, and the second
    /// compiles nothing.
    #[test]
    fn two_requests_serving_one_file_share_the_one_constant_string() {
        let engine = Engine::builder().build();
        let path = std::env::temp_dir().join(format!("dax-{}-shared.php", std::process::id()));
        assert!(std::fs::write(&path, b"<?php echo 'shared';").is_ok());
        let name = path.to_string_lossy().as_bytes().to_vec();
        for _ in 0..2 {
            let mut sink = Kept::default();
            let observables = engine.serve(
                RequestDescription {
                    sapi_name: String::from("cli"),
                    script_name: Some(name.clone()),
                    ..RequestDescription::default()
                },
                &mut sink,
            );
            assert_eq!(String::from_utf8_lossy(&sink.body), "shared");
            assert_eq!(observables.exit_code, 0);
        }
        assert_eq!(
            interned(&engine),
            1,
            "one string for the one constant, however many requests read it"
        );
        // What the cache holds after both: one unit, and the constant
        // string its `ECHO` names is the one allocation.
        let (Some(Ok(one)), Some(Ok(again))) = (
            engine
                .shared
                .cached(&name, Path::new(""), StackLimit::UNCHECKED),
            engine
                .shared
                .cached(&name, Path::new(""), StackLimit::UNCHECKED),
        ) else {
            return;
        };
        assert!(Arc::ptr_eq(&one, &again), "one published unit");
        let named = |unit: &Arc<Unit>| {
            unit.main
                .literals
                .first()
                .copied()
                .and_then(php_heap::Zval::as_str)
        };
        let (Some(first), Some(second)) = (named(&one), named(&again)) else {
            return;
        };
        assert_eq!(first.bytes(), b"shared");
        assert!(first.is_interned(), "a published constant is interned");
        assert!(first.is_same(second), "one `zend_string`, read by both");
        let _ = std::fs::remove_file(&path);
    }

    /// A tree of files for one test, under the system's temporary
    /// directory and named for the process and the test, served as the
    /// CLI serves `php main.php` from that directory; removed after.
    struct Files {
        dir: PathBuf,
    }

    impl Files {
        fn new(name: &str, files: &[(&str, &str)]) -> Files {
            let dir = std::env::temp_dir().join(format!("dax-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            assert!(std::fs::create_dir_all(&dir).is_ok());
            for (file, text) in files {
                assert!(std::fs::write(dir.join(file), text).is_ok());
            }
            Files { dir }
        }

        /// `php <dir>/<name>` in the tree's directory, the path as
        /// upstream's runner hands it: what the request wrote and how it
        /// ended.
        fn serve(&self, engine: &Engine, name: &str) -> (String, Observables) {
            let mut sink = Kept::default();
            let observables = engine.serve(
                RequestDescription {
                    sapi_name: String::from("cli"),
                    script_name: Some(self.dir.join(name).as_os_str().as_encoded_bytes().to_vec()),
                    cwd: self.dir.clone(),
                    ..RequestDescription::default()
                },
                &mut sink,
            );
            (
                String::from_utf8_lossy(&sink.body).into_owned(),
                observables,
            )
        }
    }

    impl Drop for Files {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The once forms over a second file (#396): the file is served the
    /// first time and answers what it returned, and `true` every time
    /// after, from either once form, as the oracle prints for these
    /// files.
    #[test]
    fn a_once_form_serves_a_file_once_and_answers_true_after() {
        let files = Files::new(
            "once",
            &[
                ("b.php", "<?php\necho \"b\\n\"; return 7;\n"),
                (
                    "main.php",
                    "<?php\nvar_dump(include_once \"b.php\", include_once \"b.php\", require_once \"b.php\");\n",
                ),
            ],
        );
        let (body, observables) = files.serve(&Engine::builder().build(), "main.php");
        assert_eq!(body, "b\nint(7)\nbool(true)\nbool(true)\n");
        assert_eq!(observables.exit_code, 0);
    }

    /// An include inside a function reads and writes the function's
    /// variables (#396): `zend_rebuild_symbol_table` gives the frame a
    /// table, the included file attaches to it, and what it wrote to
    /// `$r` is in the function's CV when the include returns, as the
    /// oracle prints for these files.
    #[test]
    fn an_include_inside_a_function_shares_the_function_s_variables() {
        let files = Files::new(
            "func",
            &[
                (
                    "fb.php",
                    "<?php\necho \"p=$p\\n\"; $r = 3; return \"ok\";\n",
                ),
                (
                    "main.php",
                    "<?php\nfunction f($p) { $q = include \"fb.php\"; return \"$p|$q|$r\"; }\nvar_dump(f(\"z\"));\n",
                ),
            ],
        );
        let (body, observables) = files.serve(&Engine::builder().build(), "main.php");
        assert_eq!(body, "p=z\nstring(6) \"z|ok|3\"\n");
        assert_eq!(observables.exit_code, 0);
    }

    /// A variable an included file writes at the top level is the
    /// includer's, and stands in `EG(symbol_table)` after every name the
    /// main op array attached at its start (#396), so
    /// `shutdown_destructors`' reverse walk serves it first, then the
    /// main file's own in reverse: `d2`, `d3`, `d1`, as the oracle
    /// prints for these files.
    #[test]
    fn a_variable_an_included_file_wrote_is_destructed_in_php_s_order() {
        let files = Files::new(
            "dtor",
            &[
                ("db.php", "<?php\n$b = new A(2);\n"),
                (
                    "main.php",
                    "<?php\nclass A { public $n; function __construct($n) { $this->n = $n; } function __destruct() { echo \"d{$this->n}\\n\"; } }\n$a = new A(1);\ninclude \"db.php\";\n$c = new A(3);\necho \"end\\n\";\n",
                ),
            ],
        );
        let (body, observables) = files.serve(&Engine::builder().build(), "main.php");
        assert_eq!(body, "end\nd2\nd3\nd1\n");
        assert_eq!(observables.exit_code, 0);
    }

    /// A file the parser refuses is the `ParseError` the includer may
    /// catch, at the file's own line (#396); a plain `include` whose
    /// compile was refused is not in the included-files table, so a
    /// once form after it compiles the file again and the uncaught
    /// `ParseError` is rendered at its level, as the oracle prints for
    /// these files.
    #[test]
    fn a_parse_error_in_an_included_file_is_a_parse_error_the_includer_may_catch() {
        let files = Files::new(
            "parse",
            &[
                ("bad.php", "<?php\n$x = ;\n"),
                (
                    "main.php",
                    "<?php\ntry { include \"bad.php\"; } catch (ParseError $e) { echo $e->getMessage(), \" at line \", $e->getLine(), \"\\n\"; }\necho \"after\\n\";\nvar_dump(include_once \"bad.php\");\n",
                ),
            ],
        );
        let (body, observables) = files.serve(&Engine::builder().build(), "main.php");
        let bad = files.dir.join("bad.php");
        assert_eq!(
            body,
            format!(
                "syntax error, unexpected token \";\" at line 2\nafter\n\nParse error: syntax error, unexpected token \";\" in {} on line 2\n",
                bad.canonicalize().unwrap_or(bad).display()
            )
        );
        assert_eq!(observables.exit_code, 255);
    }

    /// What compiling a file raised is raised on every request that
    /// serves it, the cache hit as the miss, in the unit's own file and
    /// at the compiler's line, before the op array writes anything: what
    /// the oracle does with no cache, and what opcache does on a hit
    /// with `opcache.record_warnings` on. `error_reporting` set from
    /// the ini masks it as it masks a runtime warning, since the path
    /// is one.
    #[test]
    fn a_cached_unit_raises_what_compiling_it_raised_on_every_request() {
        let engine = Engine::builder().build();
        let path = std::env::temp_dir().join(format!("dax-{}-nan.php", std::process::id()));
        assert!(std::fs::write(&path, b"<?php\necho 'a';\necho NAN . 'z';").is_ok());
        let name = path.to_string_lossy().as_bytes().to_vec();
        let described = || RequestDescription {
            sapi_name: String::from("cli"),
            script_name: Some(name.clone()),
            ..RequestDescription::default()
        };
        for _ in 0..2 {
            let mut sink = Kept::default();
            engine.serve(described(), &mut sink);
            assert_eq!(
                String::from_utf8_lossy(&sink.body),
                format!(
                    "\nWarning: unexpected NAN value was coerced to string in {} on line 3\naNANz",
                    path.to_string_lossy()
                )
            );
        }
        let mut masked = Kept::default();
        Engine::builder()
            .ini("error_reporting", "0")
            .build()
            .serve(described(), &mut masked);
        assert_eq!(String::from_utf8_lossy(&masked.body), "aNANz");
        let _ = std::fs::remove_file(&path);
    }

    /// What `memory_get_usage()` reads (design §5.13) after a request
    /// that built strings: a request that compiles for itself interns
    /// into its own memory, and the next request starts where the first
    /// one did, because the first request's end took every byte of it
    /// (design §5.2). The native that asks is #165; the accountant it
    /// asks is this one.
    #[test]
    fn what_memory_get_usage_reads_starts_where_it_started_for_every_request() {
        let engine = Engine::builder().build();
        let source = b"<?php echo 'a constant string long enough to be allocated';";
        let mut started_at = Vec::new();
        for _ in 0..2 {
            let mut req = Request::new(Arc::clone(&engine.shared), description(""));
            started_at.push(req.memory_usage(false));
            let compiled = compile(
                source,
                Origin {
                    filename: b"witness.php",
                    cwd: Path::new(""),
                },
                CompileIni {
                    precision: 14,
                    short_open_tag: true,
                },
                Declared::default(),
                StackLimit::UNCHECKED,
                req.heap_mut(),
                &engine.shared.map_ptr_last,
            );
            assert!(compiled.is_ok(), "the source compiles");
            assert!(
                req.memory_usage(false) > 0,
                "what the request interned is the request's own memory"
            );
        }
        // The object store's table and the VM stack's first page, and
        // nothing else, before a line runs (design §5.4, §5.6): 1024
        // buckets of a pointer each, and `ZEND_VM_STACK_PAGE_SIZE`.
        assert_eq!(
            started_at,
            vec![8192 + 262_144, 8192 + 262_144],
            "every request starts where the one before it did"
        );
        assert_eq!(
            interned(&engine),
            0,
            "and the engine's table never heard of it"
        );
    }

    /// A source the host handed over is the request's alone, as php's
    /// `-r` and `eval` are: its constant strings are interned into the
    /// request's own memory and the engine's table never hears of them.
    #[test]
    fn what_the_host_handed_over_interns_into_the_request_and_not_the_engine() {
        let engine = Engine::builder().build();
        let (body, _) = served(&engine, "<?php echo 'inline';");
        assert_eq!(body, "inline");
        assert_eq!(
            interned(&engine),
            0,
            "nothing a request compiled for itself outlives it"
        );
    }

    #[test]
    fn the_call_sequence_reaches_a_native_and_writes_what_it_returned() {
        let engine = Engine::builder().build();
        let (body, _) = served(&engine, "<?php var_dump(extension_loaded('Core'));");
        assert_eq!(body, "bool(true)\n");
        let (body, _) = served(&engine, "<?php var_dump(extension_loaded('nope'));");
        assert_eq!(body, "bool(false)\n");
    }

    /// The four arithmetic opcodes end to end, each snippet the one its
    /// kernel's witness pins, so that what the fences claim is what this
    /// engine writes and not only what the oracle wrote.
    #[test]
    fn the_arithmetic_opcodes_write_what_their_witnesses_pin() {
        let engine = Engine::builder().build();
        for (source, pinned) in [
            (
                "<?php echo 1 + (2 + extension_loaded('Core')), \"|\", 1.5 + (2.5 + extension_loaded('Core')), \"|\", \"1\" + (1 + extension_loaded('Core')), \"|\", 9223372036854775807 + (1 + extension_loaded('Core'));",
                "4|5|3|9.2233720368548E+18",
            ),
            (
                "<?php echo 1 - (2 - extension_loaded('Core')), \"|\", 1.5 - (2.5 - extension_loaded('Core')), \"|\", \"3\" - (1 + extension_loaded('Core')), \"|\", -9223372036854775807 - (1 + extension_loaded('Core'));",
                "0|0|1|-9.2233720368548E+18",
            ),
            (
                "<?php echo 2 * (3 * extension_loaded('Core')), \"|\", 1.5 * (2.5 * extension_loaded('Core')), \"|\", \"3\" * (1 + extension_loaded('Core')), \"|\", 4611686018427387904 * (1 + extension_loaded('Core'));",
                "6|3.75|6|9.2233720368548E+18",
            ),
            (
                "<?php echo 6 / (1 + extension_loaded('Core')), \"|\", 7.5 / (1.5 * extension_loaded('Core')), \"|\", \"7\" / (1 + extension_loaded('Core')), \"|\", (-9223372036854775807 - extension_loaded('Core')) / (0 - extension_loaded('Core'));",
                "3|5|3.5|9.2233720368548E+18",
            ),
        ] {
            let (body, observables) = served(&engine, source);
            assert_eq!(body, pinned, "{source}");
            assert_eq!(observables.fatal, None, "{source}");
        }
    }

    /// `div_function`: the divisor of zero php refuses to fold, so the
    /// opcode throws `DivisionByZeroError` where it stands, on line 1 of
    /// a source whose `<?php` and whose `echo` share it. The witness on
    /// [`crate::ops::div`] pins the oracle's rendering of the display branch;
    /// what this holds is the log branch's spelling of the same fatal,
    /// the status, and the double divisor the witness does not reach.
    #[test]
    fn a_division_by_zero_throws_php_s_error_where_the_opcode_stands() {
        let engine = Engine::builder().build();
        let (_, observables) = served(&engine, "<?php echo 1/0;");
        assert_eq!(
            observables.fatal.as_deref(),
            Some(
                "PHP Fatal error:  Uncaught DivisionByZeroError: Division by zero \
                 in witness.php:1\nStack trace:\n#0 {main}\n  thrown in witness.php on line 1"
            )
        );
        assert_eq!(observables.exit_code, 255);
        let (_, observables) = served(&engine, "<?php echo 1/0.0;");
        assert_eq!(
            observables
                .fatal
                .as_deref()
                .map(|text| text.contains("Uncaught DivisionByZeroError: Division by zero")),
            Some(true)
        );
    }

    /// `zend_binop_error`: an operand php will not read as a number is a
    /// `TypeError` naming php's types around the operator as written, and
    /// one with a number in front of it is the warning and the number.
    #[test]
    fn an_operand_php_refuses_to_read_as_a_number_is_php_s_type_error() {
        let engine = Engine::builder().build();
        let (_, observables) = served(&engine, "<?php echo \"abc\" + extension_loaded('Core');");
        assert_eq!(
            observables.fatal.as_deref().map(|text| text
                .contains("Uncaught TypeError: Unsupported operand types: string + bool")),
            Some(true),
            "{:?}",
            observables.fatal
        );
        let (body, observables) = served(
            &engine,
            "<?php echo \"1abc\" + (1 + extension_loaded('Core'));",
        );
        assert_eq!(
            body,
            "\nWarning: A non-numeric value encountered in witness.php on line 1\n3"
        );
        assert_eq!(observables.fatal, None);
    }

    #[test]
    fn echo_writes_each_part_as_php_converts_it() {
        let engine = Engine::builder().build();
        let (body, _) = served(
            &engine,
            "<?php echo 1, \"|\", true, \"|\", null, \"|\", 2.5;",
        );
        assert_eq!(body, "1|1||2.5");
    }

    /// `zend_exception_error` through `php_error_cb`: the four lines the
    /// witness beside [`Request::uncaught`] pins against the oracle, with
    /// the throw site in php's two spellings.
    #[test]
    fn a_call_of_a_name_nothing_declares_ends_the_request_as_php_names_it() {
        let engine = Engine::builder().build();
        let (body, observables) = served(&engine, "<?php\nnope();");
        assert_eq!(
            body,
            "\nFatal error: Uncaught Error: Call to undefined function nope() \
             in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2\n"
        );
        assert_eq!(
            observables.fatal.as_deref(),
            Some(
                "PHP Fatal error:  Uncaught Error: Call to undefined function nope() \
                 in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2"
            )
        );
        assert_eq!(observables.exit_code, 255);
    }

    /// A throwable a native left pending names the native's call as its
    /// site, and the native's own frame stands above `{main}`.
    #[test]
    fn a_throwable_a_native_left_pending_names_the_natives_call() {
        let engine = Engine::builder().build();
        let (body, observables) = served(&engine, "<?php\nvar_dump();");
        assert_eq!(
            body,
            "\nFatal error: Uncaught ArgumentCountError: var_dump() expects at least 1 argument, \
             0 given in witness.php:2\nStack trace:\n#0 witness.php(2): var_dump()\n#1 {main}\n  \
             thrown in witness.php on line 2\n"
        );
        assert_eq!(observables.exit_code, 255);
    }

    /// The fatal is `php_error_cb`'s to render, once, on the streams the
    /// two ini entries name; `serve` reports its log form and writes
    /// nothing of its own.
    #[test]
    fn a_fatal_is_displayed_or_logged_as_the_ini_entries_say() {
        let mut logged = Kept::default();
        let observables = Engine::builder()
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\nnope();"), &mut logged);
        assert!(logged.body.is_empty(), "display_errors is off");
        assert_eq!(
            logged
                .log()
                .first()
                .map(|entry| String::from_utf8_lossy(entry).into_owned()),
            observables.fatal,
            "what the log holds is the fatal, once"
        );
        assert_eq!(logged.log().len(), 1, "the four lines are one entry");

        let mut masked = Kept::default();
        Engine::builder()
            .ini("error_reporting", "0")
            .build()
            .serve(description("<?php\nnope();"), &mut masked);
        assert!(masked.body.is_empty(), "E_ERROR is masked out");
        assert!(masked.log().is_empty(), "log_errors is off");
    }

    /// `php_error_cb` builds one diagnostic with one `spprintf` and gives
    /// it to `php_log_err` once, the backtrace inside it, and
    /// `php_log_err_with_severity` stamps one date on it and writes it
    /// as one message (`Z/main/main.c:1459`). So the sink sees one call
    /// for the four-line fatal, and one call per warning for two
    /// warnings, with no terminator on any of them.
    #[test]
    fn the_sink_is_given_one_call_per_diagnostic_whatever_its_lines() {
        let mut fatal = Kept::default();
        Engine::builder()
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\nnope();"), &mut fatal);
        assert_eq!(
            fatal.log(),
            vec![
                b"PHP Fatal error:  Uncaught Error: Call to undefined function nope() \
                   in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2"
                    .to_vec()
            ],
            "four lines, one call"
        );

        let mut warned = Kept::default();
        Engine::builder()
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\necho NAN;\necho NAN;"), &mut warned);
        assert_eq!(
            warned.log(),
            vec![
                format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2").into_bytes(),
                format!("PHP Warning:  {NAN_WARNING} in witness.php on line 3").into_bytes(),
            ],
            "two lines, two calls"
        );
    }

    #[test]
    fn an_opcode_with_no_handler_ends_the_request_naming_it() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description(""));
        let unit = Unit {
            main: php_unit::OpArray {
                oplines: vec![php_unit::Opline {
                    opcode: php_unit::Opcode::DeclareAnonClass,
                    ..php_unit::Opline::default()
                }]
                .into_boxed_slice(),
                ..php_unit::OpArray::default()
            },
            ..Unit::default()
        };
        let frame = req.pushed(Arc::new(unit));
        assert_eq!(
            crate::tier::execute::<Nop>(&mut req, frame),
            Err(Fatal::Error(String::from(
                "InternalError: ZEND_DECLARE_ANON_CLASS has no handler in this engine yet"
            )))
        );
    }

    /// Design §4.3 seam 8: `Site::Entry` is raised for the entry frame of
    /// an `execute` whatever the caller is, and `Site::Return` for no
    /// entry frame, which the stack's entry slot says.
    #[test]
    fn the_loop_asks_the_tier_at_the_entry_and_at_no_entry_frames_return() {
        let engine = Engine::builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description(""));
        let unit = engine
            .unit(b"<?php echo 1;", b"witness.php")
            .unwrap_or_default();
        let unit = Unit::clone(&unit);
        let frame = req.pushed(Arc::new(unit));
        assert_eq!(crate::tier::execute::<Probe>(&mut req, frame), Ok(()));
        assert_eq!(
            req.state::<Asked>().sites,
            vec![(Site::Entry, frame)],
            "the entry frame is activated, and its own return raises no site"
        );
    }

    /// Design §5.6, §8: a user frame is activated after its `RECV*`
    /// prologue, so `Probe` sees `(Entry, callee)` once per call and
    /// `Return` once per callee frame; a call whose `RECV` throws is
    /// reported `Threw` before the activation, so no `Entry` is raised
    /// for it (#309).
    #[test]
    fn a_call_activates_its_frame_once_and_a_recv_that_throws_never_does() {
        let engine = Engine::builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description(""));
        let unit = engine
            .unit(
                b"<?php function f($a, $b = 1) { return $a; } f(1); f(2, 3); f();",
                b"witness.php",
            )
            .unwrap_or_default();
        req.declare_functions(&unit);
        let frame = req.pushed(unit);
        let outcome = crate::tier::execute::<Probe>(&mut req, frame);
        assert!(
            outcome.is_err(),
            "the third call's RECV throws and nothing catches it"
        );
        let callee = FrameRef(frame.0 + 1);
        assert_eq!(
            req.state::<Asked>().sites,
            vec![
                (Site::Entry, frame),
                (Site::Entry, callee),
                (Site::Return(crate::tier::SafepointIdx(1)), callee),
                (Site::Entry, callee),
                (Site::Return(crate::tier::SafepointIdx(1)), callee),
            ],
            "two calls activated and returned, the third never activated"
        );
    }

    /// `zend_ini_parse_quantity` (`Z/Zend/zend_ini.c:613-841`), which is
    /// how `opcache.opt_debug_level=0x10000` is a bit and not a zero: a
    /// base prefix is read as `strtoul` with base zero reads it, a
    /// leading `0` before a digit is octal, a `K`, `M` or `G` multiplies,
    /// and every shape php interprets as zero for backward compatibility
    /// is zero.
    #[test]
    fn a_whole_number_ini_value_is_read_as_php_s_quantity_reads_it() {
        let read = |value: &str| ini_quantity(value.as_bytes());
        assert_eq!(
            read("0x10000"),
            0x1_0000,
            "the dump bit, as the pin writes it"
        );
        assert_eq!(
            read("  0X10 "),
            16,
            "whitespace either side, and either case"
        );
        assert_eq!(read("65536"), 65536, "the same number in decimal");
        assert_eq!(read("0777"), 511, "a leading zero before a digit is octal");
        assert_eq!(read("0o17"), 15, "and the explicit octal prefix");
        assert_eq!(read("0b101"), 5, "and the binary one");
        assert_eq!(read("-1"), -1, "the sign is read before the digits");
        assert_eq!(read("2M"), 2 << 20, "a multiplier over decimal digits");
        assert_eq!(read("0"), 0, "a bare zero is no prefix at all");
        assert_eq!(read(""), 0, "nothing at all");
        assert_eq!(read("off"), 0, "no valid leading digits");
        assert_eq!(read("0z1"), 0, "an unknown prefix");
        assert_eq!(read("0x"), 0, "no digits after the prefix");
        assert_eq!(read("09"), 0, "octal that holds no octal digit");
    }

    /// Design §6.1: what php prints on the second stream before the
    /// program runs is the op arrays as the compiler left them, and it
    /// prints them only where `opcache.opt_debug_level` carries the
    /// `0x10000` bit. The bytes are held to the oracle's by
    /// `xtask compile-identity`, which runs both binaries over one file;
    /// what is held here is the switch.
    #[test]
    fn the_dump_before_the_optimizer_is_the_op_arrays_under_the_ini_bit_and_nothing_without_it() {
        let described = description("<?php echo 1;");
        assert_eq!(
            Engine::builder().build().dump_before_optimizer(&described),
            None,
            "no opt_debug_level at all"
        );
        assert_eq!(
            Engine::builder()
                .ini("opcache.opt_debug_level", "0")
                .build()
                .dump_before_optimizer(&described),
            None,
            "the entry at its default"
        );
        let dumped = Engine::builder()
            .ini("opcache.opt_debug_level", "0x10000")
            .build()
            .dump_before_optimizer(&described)
            .unwrap_or_default();
        assert_eq!(
            dumped,
            "\n$_main:\n     ; (lines=2, args=0, vars=0, tmps=0)\n     ; (before optimizer)\n     ; witness.php:1-1\n     ; return  [] RANGE[0..0]\n0000 ECHO int(1)\n0001 RETURN int(1)\n",
            "the one op array, in php's format"
        );
        assert_eq!(
            Engine::builder()
                .ini("opcache.opt_debug_level", "0x10000")
                .build()
                .dump_before_optimizer(&description("<?php echo 1 +;")),
            None,
            "a source the compiler refused is no dump, as php's empty second stream is"
        );
    }
}
