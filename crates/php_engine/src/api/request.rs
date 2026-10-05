//! The request-environment group (design §4.4, §5.14): what a native
//! reaches through `&mut Request`, the one parameter that says a function
//! may re-enter PHP code (principle 9, ADR-0002). Ninety-eight methods,
//! `api.lock`'s largest group. The skeleton answers each from what the
//! request holds; the body bytes and the header block are kept on the
//! request and handed to the sink by `serve`, which owns the commit
//! moment; `serve` says what the request came to.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use php_heap::{
    Fatal, FrameRef, Heap, HeapHooks, Intern, MapTable, Obj, Ref, Res, ResourceType, Slot, VmStack,
    Zval, ZvalRef,
};
use php_unit::decl::{ClassDecl, ClassKind, FunctionDecl, HookKind, PropertyDecl, Visibility};
use php_unit::{CacheSlot, Opcode, Unit};

use crate::api::abi::{DimensionRead, NativeNew, ObjectHandlers, PropertyRead, PropertyTest, Std};
use crate::api::call::Function;
use crate::api::raising::{Level, Thrown};
use crate::api::values::{
    ClassId, Key, PhpArray, PhpObject, PhpRef, PhpString, Resource, TypeTag, Value,
};
pub use crate::engine::Standard;
use crate::engine::{RequestDescription, Shared};
use crate::exec::{self, TableId, Vm};
pub use crate::open_basedir::{HostPath, Refused};
use crate::output::Output;
pub use crate::virtual_cwd::Resolve;
pub(crate) use callable::{Deprecations, Refusal, compound_split};
pub use execution::Execution;
pub use unit_cache::{
    CacheStatus, CachedFile, Invalidation, PreloadStatistics, Restart, UnitCache,
};

mod autoload;
mod build_defs;
pub use build_defs::BuildDefs;
#[cfg(php_build_provider)]
pub(crate) use build_defs::PROVIDER as BUILD_PROVIDER;
pub(crate) use build_defs::build_date;
mod callable;
mod caller;
pub(crate) use caller::NativeCaller;
mod calls;
mod class_constants;
pub(crate) use class_constants::ConstantFetch;
mod class_data;
mod classes;
mod constants;
pub(crate) use constants::halt_offset_key;
mod diagnostics;
use diagnostics::{DISPLAY_ERRORS_STDERR, display_errors_mode_of, fatal_errors};
pub(crate) use diagnostics::{append_escaped, escape_html, escaped_html_sizes};
mod ending;
pub(crate) use ending::{Ending, Phase};
mod environment;
mod execution;
mod extensions;
pub use extensions::InfoRow;
mod hash_bytes;
mod include;
mod ini;
pub(crate) mod ini_files;
pub use ini::IniEntry;
pub(crate) use ini::{ini_atol, ini_truth};
pub use ini_files::IniFiles;
mod hold;
pub(crate) use hold::Hold;
mod hooks;
mod inspect;
mod lazy;
mod limits;
pub use limits::Interrupt;
mod linking;
mod load_file;
mod memory;
mod methods;
use methods::visibility_string;
mod paths;
pub(crate) use paths::is_wrapper_url;
pub use paths::{RealpathEntry, scheme_length};
mod pending;
pub(crate) use pending::Raised;
use pending::Saved;
mod post;
mod preload;
mod recording;
mod resources;
pub use resources::{ResourceDtor, Resources};
mod response;
pub(crate) use response::Sink;
pub use response::{
    BufferHandler, BufferInit, ContentType, HandlerCall, HandlerOp, HandlerStatus, OutputOp,
    StartHandler,
};
mod shutdown;
mod site_override;
mod state;
mod static_props;
mod static_vars;
mod symbol_tables;
mod table;
mod temporary;
pub use temporary::TemporaryGuard;
mod ticks;
mod traces;
mod unit_cache;
pub(crate) use unit_cache::HandlePath;
mod variables;
mod zone;
pub use post::Upload;
pub(crate) use variables::atol;
pub use variables::url_decode;
pub use variables::{BodyLimits, Input};
pub use zone::Zone;

pub(crate) use hooks::parent_hook_trampolines;
pub(crate) use lazy::Strategy;
pub(crate) use preload::{Keyed, Preloaded, Preloading, Stacked};

/// php's version, what `PHP_VERSION` and `phpversion()` say.
pub const PHP_VERSION: &str = "8.5.9";

/// `IN_GET`, `IN_SET`, `IN_UNSET` and `IN_ISSET`
/// (`Z/Zend/zend_object_handlers.c:43`, over
/// `ZEND_GUARD_PROPERTY_*`): the bit each magic accessor takes in the
/// property guard while it serves one name on one object, so that a
/// `__get` reading the name it was called for finds the guard standing
/// and falls through to the `Undefined property` warning instead of
/// reaching itself.
const IN_GET: u32 = 1 << 0;
const IN_SET: u32 = 1 << 1;
const IN_UNSET: u32 = 1 << 2;
const IN_ISSET: u32 = 1 << 3;

/// A linked extension, by identity: its `module_number`, where its
/// `zend_module_entry` stands in `module_registry`: `Core` zero, as
/// `zend_startup` registers it first, and each extension of the bundle
/// the next in startup order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExtensionId(pub(crate) u32);

impl ExtensionId {
    /// `Core`, the engine's own natives, classes, constants and ini.
    pub(crate) const CORE: ExtensionId = ExtensionId(0);
}

/// `php_format_date(format, len, ts, localtime)` (`Z/ext/date/php_date.c:882`),
/// the one function of ext/date php's `main` and its other extensions
/// call: `format` over the timestamp in the request's default zone
/// where `localtime` says, in UTC where it does not. ext/date hands it
/// to [`ExtensionBuilder::register`]; `php_log_err` stamps a file log
/// with it, and `mail.log` its lines. An engine without ext/date stamps
/// in UTC.
///
/// [`ExtensionBuilder::register`]: crate::api::abi::ExtensionBuilder::register
#[derive(Debug, Clone, Copy)]
pub struct FormatDate(pub FormatDateFn);

/// [`FormatDate`]'s function: the request, the format, the timestamp
/// and whether it is taken in the request's zone.
type FormatDateFn = fn(&mut Request, &[u8], i64, bool) -> Result<Vec<u8>, Thrown>;

/// `php_syslog(priority, "%s", message)` (`Z/main/php_syslog.c`), which
/// shares libc's one connection with userland `openlog()`: ext/standard
/// hands it to [`ExtensionBuilder::register`], and `php_log_err` sends
/// an entry through it at its priority where `error_log` is `syslog`.
/// An engine without it sends that entry to the host's log.
///
/// [`ExtensionBuilder::register`]: crate::api::abi::ExtensionBuilder::register
#[derive(Debug, Clone, Copy)]
pub struct Syslog(pub fn(&mut Request, i32, &[u8]));

pub use crate::interrupt::Periodic;

/// The process's signal record (design §5.8, revision 38), for the
/// extension that installs signal handlers: [`SIGNAL_POST`] is what its
/// `src/ffi/` handler posts a [`Siginfo`] through, [`take_signals`] what
/// its dispatch takes, and [`Interrupt::receive_signals`] how its
/// asynchronous delivery raises this request's interrupt. The record is
/// the engine's, since signal dispositions are the process's and an
/// extension holds no atomic (ADR-0005). Beside it, [`Reentry`] is how a
/// function C calls back reaches its request, lent during a call into C
/// and otherwise a [`Deferred`] call the interrupt check serves.
pub use crate::signal::{
    DEFERRED_WORDS, Deferred, Receiving, Reentry, SIGNAL_POST, Siginfo, take_signals,
};

/// Which side of [`Request::fork`] the caller stands on, and the child's
/// id the parent is handed: `php_process`'s, which the engine's one home
/// for fork calls (#1807).
pub use crate::fork::{Forked, Pid};

/// `php_select` (`Z/main/php_network.h:83`) over the host fds streams
/// were cast to with [`Cast::Select`]: the read, write and except sets
/// each cut to the fds that came ready within `timeout`, `None` to wait
/// for ever, and the count `select(2)` answers, one per fd for each set
/// it stays in; the `Err` is the `errno` of a failed call, `EINTR` among
/// them. What `mysqlnd_poll` waits on its connections' streams with, as
/// `stream_select` waits on the streams it is handed.
pub fn select(
    sets: [&mut Vec<std::os::fd::RawFd>; 3],
    timeout: Option<std::time::Duration>,
) -> std::io::Result<usize> {
    php_heap::select_fds(sets, timeout)
}

/// Why [`Request::resolve_path`] found no file: the last attempt's
/// reason, or the throwable a wrapper's `url_stat` raised.
#[derive(Debug)]
pub enum Unresolved {
    /// No file: the reason the last attempt failed with.
    Missing(std::io::Error),
    /// What a wrapper's `url_stat` threw, which ends the walk.
    Threw(Thrown),
}

/// A write a debug build stopped: the opline whose `TMP` it was, the
/// opcode serving it, the temporary's number and what the slot held.
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy)]
struct DirtyTemporary {
    opline: u32,
    opcode: Opcode,
    number: u32,
    held: php_heap::ZvalType,
}

/// One request (design §5.14): `Send`, single-threaded, dropped at the
/// end of `serve`. Everything a native may touch is a method here or on
/// a value it hands out.
#[allow(clippy::struct_excessive_bools)] // php's EG and PG flags, each its own global, read alone
pub struct Request {
    pub(crate) shared: Arc<Shared>,
    description: RequestDescription,
    /// What the program left uncaught, kept for a host that renders it.
    kept_uncaught: Option<crate::engine::Uncaught>,
    /// The request in flight, which no fork but its own happens beside
    /// (#1807, [`Request::fork`]).
    _inside: crate::fork::Inside,
    /// The calls' state ([`calls::Calls`]).
    calls: calls::Calls,
    heap: Heap,
    stack: VmStack,
    vm: Vm,
    /// The map table of design §5.14, php's `CG(map_ptr_base)`: what a
    /// published unit's map pointers index, laid down with the request,
    /// every entry `NULL` until touched. Every op array's runtime cache
    /// stands in it ([`exec::RuntimeCache`]) and nowhere else.
    map: MapTable,
    /// The state area's state ([`state::RequestState`]).
    state: state::RequestState,
    /// The ini area's state ([`ini::Ini`]).
    ini: ini::Ini,
    /// The response area's state ([`response::Response`]).
    response: response::Response,
    /// The diagnostics' state ([`diagnostics::Diagnostics`]).
    diagnostics: diagnostics::Diagnostics,
    ending: Option<Ending>,
    /// php's `EG(exit_status)`: what the process ends with, 255 written
    /// by `php_error_cb` for a fatal and the code by `exit`, whichever
    /// wrote last.
    exit_status: i32,
    resources: Resources,
    /// php's `PG(http_globals)`: a second owner of each auto-global's
    /// array that has a track of its own ([`variables::HttpGlobals`]).
    http_globals: variables::HttpGlobals,
    /// php's post state: the body's reader, what the host has handed
    /// out of it, and the files an upload wrote ([`post::Post`]).
    post: post::Post,
    /// Each auto-global made just in time and not made yet, php's
    /// `armed` on each of `CG(auto_globals)`: the first
    /// `zend_is_auto_global` of one lays it into `EG(symbol_table)`
    /// ([`Request::arm`]).
    armed: Vec<php_unit::AutoGlobal>,
    /// The symbol tables, [`TableId`] naming one: php's
    /// `EG(symbol_table)` first, the main op array's variables (design
    /// §5.6, §5.10), and after it one table per function frame that
    /// included a file, `zend_rebuild_symbol_table`'s, which goes with
    /// its frame. A table's names stand in the order they were attached
    /// or written, each once; while a frame is attached to a name the
    /// bucket points at the frame's CV ([`Symbol::Indirect`]) and the
    /// frame owns the value, and `zend_detach_symbol_table` moves the
    /// value into the bucket when the frame leaves. The first table
    /// outlives the main frame, so a variable the program wrote at the
    /// top level is still an owner while the shutdown functions are
    /// served; `shutdown_destructors` walks it in reverse before the
    /// store's walk, and `zend_shutdown_executor_values` destroys it
    /// after the extensions' `rshutdown`. `$GLOBALS`, `compact`,
    /// `extract` and `get_defined_vars` read these tables (design §5.6).
    tables: Vec<SymbolTable>,
    /// `EG(included_files)`: the resolved path of every file compiled by
    /// the request, the primary one first (`php_execute_script_ex`),
    /// then each an `INCLUDE_OR_EVAL` opened, which the once forms read
    /// before they open one (`zend_include_or_eval`), on the request's
    /// heap ([`hash_bytes::IncludedFiles`]).
    included_files: hash_bytes::IncludedFiles,
    /// `EG(symtable_cache)`: the rebuilt symbol tables' bytes a frame's
    /// end cleaned and kept for the next rebuild, at most
    /// `SYMTABLE_CACHE_SIZE`.
    symtable_cache: Vec<hash_bytes::HashBytes>,
    /// The request's constants and `PHP_SAPI` ([`constants::Constants`]).
    constants: constants::Constants,
    /// The class constants' state ([`class_constants::ClassConstants`]).
    class_constants: class_constants::ClassConstants,
    /// The methods `zend_enum_register_funcs` gives each enum a program
    /// declares, one function number each past
    /// [`crate::enums::ENUM_FUNCTIONS`], as php allocates a
    /// `zend_internal_function` per method per enum.
    enum_functions: Stacked<(ClassId, crate::enums::EnumMethod)>,
    /// `EG(fake_scope)`: the class a write that names its own scope
    /// stands in while it lasts, as `zend_update_property_ex(scope, ...)`
    /// sets it, which the property placement reads ahead of the executed
    /// scope (`get_fake_or_executed_scope`).
    fake_scope: Option<ClassId>,
    /// `EG(filename_override)` and `EG(lineno_override)`, set while a
    /// constant expression with a class for its scope is evaluated
    /// ([`site_override`]).
    site_override: Option<site_override::SiteOverride>,
    /// The static properties' slots ([`static_props::StaticProps`]).
    static_props: static_props::StaticProps,
    /// `zend_class_mutable_data` of each class the request updated or
    /// read a constant of ([`class_data`]).
    class_data: std::collections::HashMap<ClassId, class_data::ClassData>,
    /// `zend_get_property_guard` (`Z/Zend/zend_object_handlers.c:866`),
    /// what stops a magic accessor reaching itself: one word per
    /// property name per object, carrying `IN_GET`, `IN_SET`,
    /// `IN_ISSET` or `IN_UNSET` while that accessor serves that name
    /// on that object. php keeps the word on the object, in the slot
    /// after its declared properties; an object here carries what the
    /// heap lays down and nothing more, so the words are the request's,
    /// named by `spl_object_id()` and the property's name. A row is
    /// dropped the moment its word empties, which is before the object
    /// can die, so an index the store gives out again is never one an
    /// old row still names.
    guards: Vec<((php_heap::ObjectId, Vec<u8>), u32)>,
    /// The recursion guards of objects whose class uses guards, one row
    /// per walk inside one object (`ZEND_GUARD_RECURSION_*` on the
    /// word `zend_get_recursion_guard` finds), named as [`Self::guards`]
    /// names its rows and dropped the same way, when the walk leaves.
    recursion_guards: Vec<(php_heap::ObjectId, crate::api::values::RecursionGuard)>,
    /// `zend_property_info_source_list` (`Z/Zend/zend_types.h:626`),
    /// one row per reference a typed property's slot holds: the word
    /// [`php_heap::Ref::set_sources`] carries names the row, and the row
    /// lists the properties that type the cell. php hangs the list off
    /// the cell itself; the cell is the heap's and a declared type is
    /// the engine's, so the list is here and the cell carries its name.
    /// A row is dropped by `zend_reference_destroy`, which reaches
    /// [`HeapHooks::free_reference`].
    ref_sources: Vec<(usize, Vec<RefSource>)>,
    /// php's `EG(weakrefs)`: every weakly referenced object's id against
    /// the `WeakReference` and `WeakMap` objects that name it
    /// (`crate::weakrefs`).
    pub(crate) weakrefs: crate::weakrefs::Table,
    /// php's `EG(lazy_objects_store)`: what goes with each lazy object's
    /// state, by its id ([`lazy`]).
    lazy: lazy::Table,
    /// The next word [`Request::ref_add_type_source`] hands out, never
    /// reused, so a cell freed and a cell laid down at its address are
    /// never one row.
    next_ref_source: usize,
    /// The include area's state ([`include::Include`]).
    include: include::Include,
    /// The extension whose `rinit` is being served, as
    /// `zend_activate_modules` reaches it ([`Request::activating`]).
    pub(crate) activating: Option<ExtensionId>,
    /// A throwable PHP code served by a stream's kind or filter raised
    /// inside an operation whose answer has no room for it: php's
    /// `EG(exception)` standing until the native that asked for the
    /// operation returns, which is when [`Request::serve_native`] raises
    /// it, and before which no more such code is served.
    pub(crate) stream_thrown: Option<Thrown>,
    /// `BG(user_shutdown_function_names)`.
    shutdown_functions: shutdown::ShutdownFunctions,
    /// `BG(user_tick_functions)` and `EG(ticks_count)` ([`ticks::Ticks`]).
    ticks: ticks::Ticks,
    /// The autoload area's state ([`autoload::Autoload`]).
    autoload: autoload::Autoload,
    /// The environment area's state ([`environment::Environment`]).
    environment: environment::Environment,
    /// The paths area's state ([`paths::Paths`]).
    paths: paths::Paths,
    cwd: PathBuf,
    /// `sapi_get_request_time`: when the request started, since the
    /// epoch. The host keeps no time of its own, so it is the moment the
    /// request is made, as `php_time()` makes it.
    started: std::time::Duration,
    /// The fibers running on this request, innermost last, each with the
    /// execution state of the stack that resumed it, and where each
    /// suspends to (design §5.11, `crate::fiber`).
    fibers: crate::fiber::Fibers,
    /// php's `EG(exception)`: what a handler left pending, a throwable or
    /// one of php's two exits ([`Raised`]), which the loop reads at the
    /// `Threw` edge.
    raised: Option<Raised>,
    /// php's `zend_bailout` under way: a fatal was rendered where it was
    /// raised and the request is leaving for the `zend_try` that catches
    /// it, the outermost serving's end or the step of
    /// `php_request_shutdown` it was raised in ([`Request::catch_bailout`]).
    /// Nothing is served until then: the loop unwinds as it unwinds the
    /// unwind exit, past every `catch` and `finally`, a native reading
    /// what its callback left reads the fatal, and a second fatal raised
    /// on the way is not rendered.
    bailout: bool,
    /// Where the request stands ([`ending::Progress`]).
    progress: ending::Progress,
    /// Where `shutdown_destructors` caught the `zend_bailout` a
    /// destructor's raise ends the walk with
    /// (`Z/Zend/zend_execute_API.c:258-266`): the walk over the symbol
    /// table stops, and the objects then standing are marked destructed
    /// in the heap, `zend_objects_store_mark_destructed`.
    shutdown_walk_ended: bool,
    /// What the entry frame of the innermost serving returned.
    returned: Value,
    /// The interrupt, the timer and the stack limit ([`limits::Limits`]).
    limits: limits::Limits,
    /// A debug build's record of the first write of a `TMP` it found
    /// over a slot that still held an owner, until the loop takes it
    /// ([`Request::take_sensed`]).
    #[cfg(debug_assertions)]
    dirty: Option<DirtyTemporary>,
    /// The table the last `FETCH_DIM_W` laid its `IS_INDIRECT` into,
    /// for the `ASSIGN_DIM` after it to hold ([`Request::hold_walk`]).
    walk: Option<hold::Walk>,
    /// The classes the units this request served declared, each the
    /// unit that holds it and the index of its declaration, in the
    /// order declared: php's `EG(class_table)` past what the engine
    /// registered at startup. A `ClassId` past the engine's classes
    /// counts into this list. What design §6.2 binds early into an
    /// immutable linked class shared across requests is not built yet,
    /// so every class a unit declares is this request's, the
    /// `DECLARE_CLASS_DELAYED` path, and dropped with it.
    declared: Keyed<Declared>,
    /// The classes a load bound early ([`classes::Classes`]).
    classes: classes::Classes,
    /// The classes whose link is under way and what their links left
    /// for later ([`linking::Links`]).
    links: linking::Links,
    /// `EG(record_errors)` and `EG(errors)`: the diagnostics held back
    /// for a compile or a link to emit ([`recording::Record`]).
    record: recording::Record,
    /// `CG(rtd_key_counter)` over the units this request compiled for
    /// itself: the runtime definition keys and anonymous class names
    /// they made, which the next one counts on from
    /// ([`crate::engine::Shared::compile_for`]).
    pub(crate) keys_made: u32,
    /// `CG(script_encoding_list)` where an `ini_set()` of
    /// `zend.script_encoding` moved it this request; the engine's where
    /// none did.
    pub(crate) script_encodings: Option<Vec<php_compile::multibyte::Encoding>>,
    /// The aliases `class_alias()` added to `EG(class_table)`, each the
    /// bucket it stands in, the lowered name it is keyed by and the
    /// class it names: `ZVAL_ALIAS_PTR` rows, request state as the
    /// classes the request declared are.
    aliases: Keyed<preload::Alias>,
    /// `EG(class_table)`'s buckets past the engine's classes.
    buckets: preload::Buckets,
    /// The functions and methods the units this request served declared,
    /// each the unit that holds it and which of its op arrays is the
    /// body: php's `EG(function_table)` past the natives and each user
    /// class's `function_table`. A `Function` past the natives and the
    /// native methods counts into this list (design §5.6).
    user_functions: Stacked<UserFunction>,
    /// The `static` variables' tables ([`static_vars::StaticVars`]).
    static_vars: static_vars::StaticVars,
    /// The `emalloc`s php's `create_object` made beside each object
    /// that has them, by the object's id ([`Request::lay_beside`]).
    beside: HashMap<u32, Vec<php_heap::Block>>,
    /// Where the request is the preload's (design §6.3), the files it
    /// compiled into the preload.
    preloading: Option<Preloading>,
    /// The unit opcache last served, `zend_accel_load_script`'s: one it
    /// held or keeps in its file cache, compiled with
    /// `ZEND_COMPILE_DELAYED_BINDING` ([`Request::declare_classes`]).
    accel_loaded: Option<Arc<Unit>>,
}

/// One row of an object's property table as [`Request::property_rows`]
/// reads it: the name, the value, and for a declared slot the class
/// that declared it and its visibility.
pub(crate) type PropertyRow = (Key, Value, Option<(ClassId, Visibility)>);

/// `DEBUG_BACKTRACE_PROVIDE_OBJECT` (`Z/Zend/zend_constants.h`): a
/// method's row carries its receiver.
pub(crate) const DEBUG_BACKTRACE_PROVIDE_OBJECT: i64 = 1;

/// `DEBUG_BACKTRACE_IGNORE_ARGS`: no row carries its arguments.
pub(crate) const DEBUG_BACKTRACE_IGNORE_ARGS: i64 = 2;

/// One bucket of a symbol table, php's zval in `EG(symbol_table)` or a
/// rebuilt one: the value the table owns, or `IS_INDIRECT`, the CV of the
/// frame the name is attached to (`zend_attach_symbol_table`), which
/// owns the value while the frame stands.
///
/// `Held` is the value the table owns kept in a place of its own: php
/// hands a fetch by name for writing the bucket's own `zval *`, and a
/// bucket here moves as the list of names grows, so the first such fetch
/// moves the value into a cell that does not ([`Request::symbol_place`]).
/// The cell is the table's alone and never a PHP reference: what it holds
/// is the bucket's value, a reference among them.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Symbol {
    Value(Value),
    Held(PhpRef),
    Indirect(FrameRef, u32),
}

/// One symbol table: its names in order, and the function frame it was
/// rebuilt for, which frees it when it pops; `EG(symbol_table)` has no
/// owner and lives as long as the request.
#[derive(Debug, Default)]
pub(crate) struct SymbolTable {
    owner: Option<FrameRef>,
    entries: Vec<(Vec<u8>, Symbol)>,
    /// What a rebuilt table holds on the request's heap
    /// ([`Request::rebuilt_table_bytes`]); `EG(symbol_table)`'s are not
    /// held.
    bytes: Option<hash_bytes::HashBytes>,
}

/// What `zend_include_or_eval` handed the handler: `ZEND_FAKE_OP_ARRAY`,
/// a once form over a path the table already had, which answers `true`;
/// `NULL`, a path nothing opened or an empty string to `eval`, which
/// answers `false`; or the unit of the file or the string, whose main op
/// array the handler serves on a frame of its own.
pub(crate) enum Opened {
    Already,
    Failed,
    Unit(Arc<Unit>),
}

/// The links of one stack, innermost first: the frames `frames` numbers
/// and the calls `calls` numbers that are being served, each frame's
/// `calls_below` saying where among the calls it was pushed.
fn links_of<'a>(
    stack: &'a VmStack,
    vm: &'a Vm,
    frames: std::ops::Range<usize>,
    calls: std::ops::Range<usize>,
    expand: &dyn Fn(PhpObject) -> Vec<Link<'a>>,
) -> Vec<Link<'a>> {
    /// The calls being served below `below`, from where the walk stands.
    fn take_below<'a>(
        served: &'a [exec::CallState],
        taken: &mut usize,
        below: usize,
        links: &mut Vec<Link<'a>>,
    ) {
        while *taken < below {
            if let Some(call) = served.get(*taken)
                && call.stage == exec::CallStage::Serving
            {
                links.push(Link::Call(call));
            }
            *taken += 1;
        }
    }
    let states = vm.frames();
    let served = vm.calls();
    let end = calls.end.min(served.len());
    let mut links: Vec<Link> = Vec::new();
    let mut taken = calls.start;
    // A placeholder standing at `at`, where a generator's frame stood,
    // and the generators it expands into, outermost first here since
    // the walk goes up the stack and is reversed at the end.
    let placed = |at: usize, taken: &mut usize, links: &mut Vec<Link<'a>>| {
        for mark in vm.placeholders().iter().filter(|mark| mark.frame == at) {
            take_below(served, taken, mark.calls.min(end), links);
            links.extend(expand(mark.generator).into_iter().rev());
        }
        for site in vm.sites().iter().filter(|site| site.frame == at) {
            take_below(served, taken, site.calls.min(end), links);
            links.push(Link::Site(site));
        }
    };
    for at in frames.clone() {
        placed(at, &mut taken, &mut links);
        let Some(state) = states.get(at) else {
            break;
        };
        let below = usize::try_from(state.calls_below)
            .unwrap_or(usize::MAX)
            .min(end);
        take_below(served, &mut taken, below, &mut links);
        if let Some(generator) = state.generator {
            links.extend(expand(generator).into_iter().rev());
        }
        if !state.leaving {
            links.push(Link::Frame(Slots::Stack(stack, at), state));
        }
    }
    placed(frames.end, &mut taken, &mut links);
    take_below(served, &mut taken, end, &mut links);
    links.reverse();
    links
}

/// One link of php's `prev_execute_data` chain as this engine holds it:
/// a user frame, with its place among the frames, or a call a native
/// is serving.
enum Link<'a> {
    Frame(Slots<'a>, &'a exec::FrameState),
    Call(&'a exec::CallState),
    /// `zend_get_attribute_object`'s dummy frame ([`exec::Site`]): user
    /// code that serves no function, has no `$this` and no scope, and
    /// makes no trace row of its own.
    Site(&'a exec::Site),
}

/// Where the slots of a frame on the chain are: on a VM stack at its
/// place, or kept by a suspended generator a running one was resumed
/// through, which php's `execute_fake` placeholder stands for until a
/// backtrace expands it.
#[derive(Clone, Copy)]
enum Slots<'a> {
    Stack(&'a VmStack, usize),
    Kept(&'a php_heap::Frame),
}

impl<'a> Slots<'a> {
    fn frame(self) -> Option<&'a php_heap::Frame> {
        match self {
            Slots::Stack(stack, at) => stack.frame(FrameRef(u32::try_from(at).unwrap_or(u32::MAX))),
            Slots::Kept(frame) => Some(frame),
        }
    }
}

/// Why `zend_get_property_offset` answered its wrong offset for a name
/// no declared instance property holds.
#[derive(Clone, Copy)]
enum Unplaced {
    /// A name led by a NUL byte (`zend_bad_property_name`).
    BadName,
    /// A `static` property the scope may not reach
    /// (`zend_bad_property_access`).
    Denied(Visibility),
}

/// What `zend_get_property_offset` places a declared instance property
/// by: the slot it sits in, the visibility a read is held to, the set
/// visibility a write is held to where it has one of its own, and
/// whether the property carries a type.
#[derive(Debug, Clone, Copy)]
struct DeclaredSlot {
    slot: u32,
    /// `property_info->ce`: the class that declared what stands in the
    /// slot, which is the scope its visibility is measured against.
    declaring: ClassId,
    visibility: Visibility,
    set_visibility: Option<Visibility>,
    typed: bool,
    /// `ZEND_ACC_READONLY`, which the set visibility's refusal names.
    readonly: bool,
    /// `property_info->prototype->ce`: the class that first declared the
    /// property up the chain, through every redeclaration that was not
    /// over a private one, which a protected property's access is
    /// measured against (`is_protected_compatible_scope`).
    root: ClassId,
}

/// What a write to a typed declared slot reads off its
/// `zend_property_info`: the class that declared it, which the
/// `TypeError` of a value the type refuses and the `Error` of a write
/// to a readonly property both name; the declared type; and whether
/// php marked it `ZEND_ACC_READONLY`.
pub(crate) struct TypedProperty {
    pub(crate) declaring: ClassId,
    pub(crate) ty: php_unit::decl::Type,
    pub(crate) readonly: bool,
}

/// One user function or method a request declared: the unit and the
/// body its op array is read from, and for a method the row of its
/// class's method table it is, from which its declaration is read. The
/// two differ for a method a trait gave a class: php copies the
/// `zend_function` into the class with the class as its scope and
/// shares the trait's opcodes (`zend_add_trait_method`), so the row is
/// the class's and the op array the trait's unit's.
pub(crate) struct UserFunction {
    pub(crate) unit: Arc<Unit>,
    pub(crate) body: exec::Body,
    pub(crate) member: Option<Member>,
    /// `ZEND_ACC_DEPRECATED` and `ZEND_ACC_NODISCARD`, which
    /// `zend_compile_func_decl` and the `#[\NoDiscard]` validator raise
    /// from the declaration's attributes, decoded where the function is
    /// declared.
    pub(crate) flags: crate::attributes::Flags,
}

/// A method as its class holds it: the class, `common.scope`, and
/// where among the class's declaration it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Member {
    pub(crate) class: ClassId,
    pub(crate) at: MemberAt,
}

/// Where a method stands in its class's declaration: among its methods,
/// `ce->function_table`, or as one of a property's hooks,
/// `prop_info->hooks[kind]`, which php's function table never lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberAt {
    Method(u32),
    Hook { property: u32, kind: HookKind },
}

/// One class a request declared: the unit that holds its declaration and
/// the declaration's index there, for a class that uses traits the
/// class composed with them at its link, which is the table's row in
/// its place (`zend_do_bind_traits`), and the bucket of
/// `EG(class_table)` it stands in; beside them its placed properties,
/// once read ([`Request::placed_properties`]).
pub(crate) struct Declared {
    unit: Arc<Unit>,
    at: u32,
    composed: Option<Box<php_unit::link::Composed>>,
    place: preload::Bucket,
    placed: OnceLock<Placed>,
}

/// A class's declared instance properties in slot order, each beside the
/// class that declared it: `ce->properties_info_table` over
/// `ce->default_properties_table` ([`Request::placed_properties`]).
pub(crate) type Placed = Box<[(ClassId, PropertyDecl)]>;

/// `zend_string_tolower` for a lookup: `find` over `name` lowered, on the
/// stack where it fits, since the key is only read, and over `name`
/// itself where it has nothing to lower.
pub(crate) fn lowered_key<R>(name: &[u8], find: impl FnOnce(&[u8]) -> R) -> R {
    if !name.iter().any(u8::is_ascii_uppercase) {
        return find(name);
    }
    let mut buffer = [0u8; 128];
    match buffer.get_mut(..name.len()) {
        Some(lowered) => {
            lowered.copy_from_slice(name);
            lowered.make_ascii_lowercase();
            find(lowered)
        }
        None => find(&name.to_ascii_lowercase()),
    }
}

impl Declared {
    /// The declaration the table holds for the class.
    fn decl(&self) -> Option<&ClassDecl> {
        match &self.composed {
            Some(composed) => Some(&composed.decl),
            None => self.unit.classes.get(usize::try_from(self.at).ok()?),
        }
    }
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
        exec::Body::Main | exec::Body::Method { .. } | exec::Body::Hook { .. } => None,
    }
}

impl Request {
    #[allow(clippy::too_many_lines)] // one line per field of the request
    pub(crate) fn new(shared: Arc<Shared>, description: RequestDescription) -> Request {
        let cwd = description.cwd.clone();
        let exit_status = description.exit_status;
        let output = Output::activate(description.web.as_ref());
        let resources = Resources::default();
        // `init_executor`'s `zend_vm_stack_init`: the first page of the
        // VM stack, out of the heap the object store's table stands in.
        // The main chunk holds it, so nothing refuses it; a heap that
        // did would refuse the first push again, with php's fatal.
        let mut heap = Heap::new();
        let mut stack = VmStack::new();
        let _laid = stack.open(&mut heap);
        let tables = preload::Tables::of(shared.units.preloaded());
        let buckets = preload::Buckets::past(tables.classes.len() + tables.aliases.len());
        let mut req = Request {
            // Read before `shared` moves in, as fields are in the order written.
            paths: paths::Paths::startup(&shared),
            shared,
            description,
            kept_uncaught: None,
            _inside: crate::fork::Inside::enter(),
            heap,
            stack,
            vm: Vm::default(),
            map: MapTable::new(),
            state: state::RequestState::default(),
            ini: ini::Ini::default(),
            diagnostics: diagnostics::Diagnostics::default(),
            response: response::Response::new(output),
            ending: None,
            exit_status,
            resources,
            http_globals: variables::HttpGlobals::default(),
            post: post::Post::default(),
            armed: Vec::new(),
            tables: vec![SymbolTable::default()],
            included_files: hash_bytes::IncludedFiles::default(),
            symtable_cache: Vec::new(),
            constants: constants::Constants::default(),
            class_constants: class_constants::ClassConstants::default(),
            enum_functions: tables.enum_functions,
            calls: calls::Calls::default(),
            fake_scope: None,
            site_override: None,
            static_props: static_props::StaticProps::default(),
            class_data: std::collections::HashMap::new(),
            guards: Vec::new(),
            recursion_guards: Vec::new(),
            ref_sources: Vec::new(),
            weakrefs: crate::weakrefs::Table::default(),
            lazy: lazy::Table::default(),
            next_ref_source: 1,
            include: include::Include::default(),
            activating: None,
            stream_thrown: None,
            shutdown_functions: shutdown::ShutdownFunctions::default(),
            ticks: ticks::Ticks::default(),
            autoload: autoload::Autoload::default(),
            environment: environment::Environment::default(),
            cwd,
            started: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default(),
            fibers: crate::fiber::Fibers::default(),
            raised: None,
            bailout: false,
            progress: ending::Progress::default(),
            shutdown_walk_ended: false,
            returned: Value::null(),
            limits: limits::Limits::new(),
            declared: tables.classes,
            classes: classes::Classes::default(),
            links: linking::Links::default(),
            record: recording::Record::default(),
            keys_made: 0,
            script_encodings: None,
            buckets,
            aliases: tables.aliases,
            user_functions: tables.functions,
            static_vars: static_vars::StaticVars::default(),
            beside: HashMap::new(),
            preloading: None,
            accel_loaded: None,
            #[cfg(debug_assertions)]
            dirty: None,
            walk: None,
        };
        req.activate();
        req
    }

    /// `php_request_startup` and `zend_activate` over a request just
    /// laid: the host's constant, each extension's request state, the
    /// timer, the memory limit, the collector and the error level.
    fn activate(&mut self) {
        self.register_sapi_constant();
        self.build_request_states();
        // The execution timer, as `zend_set_timeout` arms one at the
        // start of a request (design §5.8), and the memory limit, as
        // `php_request_startup` sets one from the ini.
        self.arm_timer();
        self.set_memory_limit();
        // `zend.enable_gc` as the startup ini leaves the collector
        // (`OnUpdateGCEnabled`), and `gc_reset` from `zend_activate`.
        let enable_gc = ini_truth(self.ini_get(b"zend.enable_gc"));
        self.heap.gc_startup(enable_gc);
        self.update_error_reporting();
        self.start_stack_limit();
    }

    // -------------------------------------------------- crate-internal

    pub(crate) fn stack_mut(&mut self) -> &mut VmStack {
        &mut self.stack
    }

    /// [`VmStack::move_to_heap`] over the request's stack and heap.
    pub(crate) fn move_frame_to_heap(&mut self) -> Result<Option<php_heap::Frame>, Fatal> {
        self.stack.move_to_heap(&mut self.heap)
    }

    /// The first page of the VM stack the request runs on, out of its
    /// heap, where none stands: what `zend_fiber_execute` takes for a
    /// fiber's stack before the fiber's function is called.
    pub(crate) fn open_stack(&mut self) -> Result<(), Fatal> {
        self.stack.open(&mut self.heap)
    }

    /// The engine's half of the frames: what the loop and the handlers
    /// read (design §5.6).
    pub(crate) fn vm(&self) -> &Vm {
        &self.vm
    }

    pub(crate) fn vm_mut(&mut self) -> &mut Vm {
        &mut self.vm
    }

    /// The fibers running on this request (design §5.11).
    pub(crate) fn fibers(&self) -> &crate::fiber::Fibers {
        &self.fibers
    }

    pub(crate) fn fibers_mut(&mut self) -> &mut crate::fiber::Fibers {
        &mut self.fibers
    }

    /// The half of `zend_fiber_switch_context` that is the engine's:
    /// the execution state the request runs on and `other` change
    /// places, `zend_fiber_capture_vm_state` and
    /// `zend_fiber_restore_vm_state` as one swap. The VM stack carries
    /// the current frame and the entry slot with it, so neither is
    /// saved on its own.
    pub(crate) fn switch_execution(&mut self, other: &mut crate::fiber::Execution) {
        // The execution left off the running chain keeps what it was
        // lent ([`exec::Receiver`]).
        self.own_lent_receivers();
        std::mem::swap(&mut self.stack, &mut other.stack);
        std::mem::swap(&mut self.vm, &mut other.vm);
        self.swap_error_reporting(&mut other.error_reporting);
        self.swap_stack_limit(&mut other.stack_limit);
    }

    /// The stacks php's one `prev_execute_data` chain runs through: the
    /// one the request is running on, then each resumer's below it, the
    /// request's own last.
    fn executions(&self) -> impl Iterator<Item = (&VmStack, &Vm)> {
        std::iter::once((&self.stack, &self.vm)).chain(
            self.fibers
                .below()
                .map(|execution| (&execution.stack, &execution.vm)),
        )
    }

    /// The innermost user frame on the chain, which is where
    /// `zend_get_executed_filename` and `zend_get_executed_lineno` stop:
    /// on a fiber whose callable is a native, the resumer's.
    fn innermost_frame(&self) -> Option<&exec::FrameState> {
        self.executions().find_map(|(_, vm)| vm.innermost())
    }

    /// The loop over a generator's frame, bound to the engine's tier
    /// ([`crate::tier::resume`]).
    pub(crate) fn resume_loop(&self) -> fn(&mut Request, FrameRef) -> Result<(), Fatal> {
        self.shared.resume
    }

    /// Push a frame for `unit`'s op array: its shape on the VM stack and
    /// its engine state, which are pushed and popped together and share a
    /// `FrameRef`. A VM stack whose page the memory limit refuses is
    /// the fatal php raises there, and nothing is pushed.
    pub(crate) fn push_frame(&mut self, unit: Arc<Unit>) -> Result<FrameRef, Fatal> {
        let code = exec::code_of(&unit);
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        let frame = self.stack.push_frame(&mut self.heap, slots, 0)?;
        exec::init_runtime_cache(&mut self.map, code, exec::Body::Main);
        self.forget_main_statics(code.runtime_cache);
        self.vm.push(unit);
        // `zend_execute` gives the main op array's frame `EG(symbol_table)`
        // and `i_init_code_execute_data` attaches it, so every CV name
        // stands in the table, in the compiler's order, from the start.
        if let Some(state) = self.vm.at_mut(frame) {
            state.symbol_table = Some(TableId::GLOBAL);
        }
        self.attach_symbol_table(frame);
        Ok(frame)
    }

    /// The frame for the main op array of a file an `INCLUDE_OR_EVAL`
    /// opened, `zend_vm_stack_push_call_frame` under
    /// `ZEND_CALL_NESTED_CODE | ZEND_CALL_HAS_SYMBOL_TABLE` and
    /// `i_init_code_execute_data` (`Z/Zend/zend_vm_def.h:6590`,
    /// `Z/Zend/zend_execute.c:4504`): the includer's receiver and called
    /// scope carried over, the receiver with one more owner since the pop
    /// gives one up, the includer's scope written onto the op array
    /// (`new_op_array->scope`), and the includer's symbol table shared,
    /// rebuilt for it first where it is a function frame with none
    /// ([`Request::rebuild_symbol_table`]); then the new frame attached
    /// to it, so the included code reads the includer's variables.
    /// `caller` is `prev_execute_data`, the frame whose opline is the
    /// `INCLUDE_OR_EVAL`, and `returns_to` its `VAR` where the value is
    /// read. The push comes first, as php's does, so a VM stack page the
    /// memory limit refuses is the fatal and nothing else is touched.
    pub(crate) fn push_code_frame(
        &mut self,
        unit: Arc<Unit>,
        caller: FrameRef,
        returns_to: Option<(FrameRef, u32)>,
    ) -> Result<FrameRef, Fatal> {
        let code = exec::code_of(&unit);
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        let frame = self.stack.push_frame(&mut self.heap, slots, 0)?;
        let (this, called_scope, scope) = self.vm.at(caller).map_or((None, None, None), |state| {
            (state.this(), state.called_scope, self.frame_scope(caller))
        });
        // `ZEND_CALL_HAS_THIS` carried from the including frame, whose
        // `$this` that frame keeps: no `ZEND_CALL_RELEASE_THIS`.
        let receiver = this.map(exec::Receiver::lent);
        let table = self.rebuild_symbol_table(caller);
        exec::init_runtime_cache(&mut self.map, code, exec::Body::Main);
        self.forget_main_statics(code.runtime_cache);
        self.vm.push_body(
            unit,
            exec::Body::Main,
            None,
            receiver,
            called_scope,
            0,
            returns_to,
            exec::Caller::Opline(caller),
        );
        if let Some(state) = self.vm.at_mut(frame) {
            state.scope = scope;
            state.symbol_table = Some(table);
        }
        self.attach_symbol_table(frame);
        Ok(frame)
    }

    /// The op array `frame` serves held on the request's heap as php's
    /// `INCLUDE_OR_EVAL` holds it ([`exec::HeapOpArray`]): `copy`, made
    /// before the frame was pushed, and the runtime cache
    /// `i_init_code_execute_data` lays down after the symbol table is
    /// attached, `cache_size` bytes. Both go when the frame is popped,
    /// whether the file returned or threw, and an `eval`'s as an
    /// include's:
    ///
    /// ```php
    /// $f = __DIR__ . '/' . bin2hex(random_bytes(8)) . '.php';
    /// file_put_contents($f, '<?php if ($t) throw new Exception("x"); $y = 2; return $y;'); touch($f, time() - 10); clearstatcache();
    /// $t = false; include $f; $t = true; try { include $f; } catch (Exception $e) {} unset($e);
    /// $m = memory_get_usage(); try { include $f; } catch (Exception $e) {} unset($e); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $t = false; $r = include $f; echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $r = eval('$q = 1; return $q;'); echo memory_get_usage() - $m, "\n";
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 0
    /// 0
    /// 0
    /// ```
    ///
    /// The `Err` is the memory limit, the copy held already.
    pub(crate) fn hold_op_array(
        &mut self,
        frame: FrameRef,
        copy: crate::api::values::Emalloc,
    ) -> Result<(), Thrown> {
        let size = self
            .vm
            .serving(frame)
            .and_then(|serving| serving.code().map(|code| code.cache_size))
            .unwrap_or(0);
        if let Some(state) = self.vm.at_mut(frame) {
            state.op_array = Some(exec::HeapOpArray {
                copy,
                run_time_cache: None,
            });
        }
        let cache =
            crate::api::values::Emalloc::new(self, usize::try_from(size).unwrap_or(usize::MAX))?;
        if let Some(held) = self
            .vm
            .at_mut(frame)
            .and_then(|state| state.op_array.as_mut())
        {
            held.run_time_cache = Some(cache);
        }
        Ok(())
    }

    /// `zend_leave_helper`'s arm for a nested code frame
    /// (`Z/Zend/zend_vm_def.h:3030`): the frame's CVs detached into the
    /// table it shares with its includer, the frame popped, and the
    /// includer attached to the table again, so what the included file
    /// wrote to a variable of the includer's is in the includer's CV
    /// when the loop goes on there. A function frame is popped alone,
    /// as `zend_leave_helper`'s other arms pop one.
    pub(crate) fn leave_frame(&mut self, frame: FrameRef) {
        let code_frame = self
            .vm
            .at(frame)
            .is_some_and(|state| state.function.is_none());
        if code_frame {
            self.detach_symbol_table(frame);
        }
        self.pop_frame();
        if code_frame {
            let caller = self.stack.current();
            self.attach_symbol_table(caller);
        }
    }

    /// `CACHED_PTR(num)`: what the runtime cache of the op array `frame`
    /// serves holds at `slot`, in this request's map table
    /// ([`exec::Vm::cached`]).
    pub(crate) fn cached(&self, frame: FrameRef, slot: CacheSlot) -> exec::Cached {
        self.vm.cached(&self.map, frame, slot)
    }

    /// `CACHE_PTR(num, ptr)`: `value` into the runtime cache of the op
    /// array `frame` serves at `slot`, in this request's map table
    /// ([`exec::Vm::prime`]), where every pass after reads it.
    pub(crate) fn prime(&mut self, frame: FrameRef, slot: CacheSlot, value: exec::Cached) {
        self.vm.prime(&mut self.map, frame, slot, value);
    }

    /// Push a frame for the body of the user function `function`, the
    /// call that reached it served (`zend_vm_stack_push_call_frame` and
    /// `i_init_func_execute_data`, design §5.6): the CVs and the
    /// temporaries as slots, the arguments the call placed written into
    /// the first CVs, which are the parameters, the ones past the
    /// declared parameters kept beside the frame, and the receiver and
    /// the called scope carried over; `returns_to` is the caller's slot
    /// for what the frame returns and `caller` the frame whose opline
    /// pushed the call, `None` where a native did. The frame's room on
    /// the VM stack is php's `zend_vm_calc_used_stack`, the extra
    /// arguments' slots with it, and a page the memory limit refuses is
    /// the fatal php raises at the push; a function that is no user
    /// function is an internal error.
    pub(crate) fn push_user_frame(
        &mut self,
        function: Function,
        call: exec::CallState,
        returns_to: Option<(FrameRef, u32)>,
        caller: exec::Caller,
    ) -> Result<FrameRef, Fatal> {
        let no_body = || {
            Fatal::Error(String::from(
                "InternalError: a user function whose body the unit has not",
            ))
        };
        let (unit, body) = {
            let user = self.user_function(function).ok_or_else(no_body)?;
            (Arc::clone(&user.unit), user.body)
        };
        let code = exec::body_of(&unit, body).ok_or_else(no_body)?;
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        // The declared parameters, which is php's `common.num_args` and
        // excludes a variadic: an argument from the variadic's position
        // on is an extra one, where php leaves it on the stack for
        // `RECV_VARIADIC` to collect.
        let parameters = self.function_decl(function).map_or(0, |decl| {
            decl.parameters.iter().filter(|p| !p.variadic).count()
        });
        let num_args = u32::try_from(call.arguments.len()).unwrap_or(u32::MAX);
        let extra_args = call.arguments.len().saturating_sub(parameters);
        let frame = self.stack.push_frame(&mut self.heap, slots, extra_args)?;
        let closure = call.closure;
        exec::init_runtime_cache(&mut self.map, code, body);
        self.vm.push_body(
            unit,
            body,
            Some(function),
            call.receiver,
            call.called_scope,
            num_args,
            returns_to,
            caller,
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
            state.extra_named = call.extra_named;
            state.closure = closure;
        }
        if let Some(name) = call.name {
            self.release(Value::string(name));
        }
        for coerced in call.coerced {
            self.release(coerced);
        }
        Ok(frame)
    }

    /// The frame off both stacks, every owner its slots held given up
    /// while the frame still stands, `i_free_compiled_variables` over
    /// the CVs, each a possible root of the cycle collector as php's
    /// `i_zval_ptr_dtor` makes it, and the temporaries after them as
    /// `FREE_OP` gives one up, since a temporary the compiler numbered
    /// for an opline's value and nothing read is still an owner here;
    /// then, in `zend_leave_helper`'s order, the extra named arguments,
    /// the extra arguments (`zend_vm_stack_free_extra_args`, `nogc`),
    /// the receiver (`ZEND_CALL_RELEASE_THIS`) and the closure, all of
    /// them while the frame still stands on both stacks, as php's
    /// `vm_stack_top` is only put back after them, so a destructor one
    /// of them serves pushes its frames above it; then
    /// `zend_vm_stack_free_call_frame`. The frame is off the chain a
    /// backtrace reads from the first release on, as php's
    /// `EG(current_execute_data)` is the caller's by then, so a
    /// destructor served from here is called from the caller's line:
    ///
    /// ```php
    /// class D { function __destruct() { debug_print_backtrace(); } }
    /// function f($x) { $d = new D; }
    /// f(new D);
    /// ```
    /// ```expect
    /// #0 %s(4): D->__destruct()
    /// #0 %s(4): D->__destruct()
    /// ```
    pub(crate) fn pop_frame(&mut self) {
        let frame = self.stack.current();
        if let Some(state) = self.vm.at_mut(frame) {
            state.leaving = true;
        }
        let slots = self.stack.frame(frame).map_or(0, php_heap::Frame::len);
        let cvs = self
            .vm
            .serving(frame)
            .and_then(|serving| serving.code().map(|code| code.num_cvs as usize))
            .unwrap_or(slots);
        for at in 0..slots {
            let value = self.take_slot(frame, u32::try_from(at).unwrap_or(u32::MAX));
            if at < cvs {
                self.release(value);
            } else {
                self.release_nogc(value);
            }
        }
        self.free_rebuilt_table(frame);
        // A generator's frame leaves its closure to the generator
        // (`ZendGenerator::closure`), which keeps it until it is freed.
        let held = self.vm.at_mut(frame).map(|state| {
            (
                std::mem::take(&mut state.extra_named),
                std::mem::take(&mut state.extra_args),
                state.receiver.take(),
                state.closure.take().filter(|_| state.generator.is_none()),
            )
        });
        if let Some((named, extra_args, receiver, closure)) = held {
            self.release_named(named);
            for extra in extra_args {
                self.release_nogc(extra);
            }
            if let Some(receiver) = receiver {
                self.give_up_receiver(receiver);
            }
            if let Some(closure) = closure {
                self.release(closure.as_value());
            }
        }
        // `destroy_op_array`, which frees the heap runtime cache, then
        // `efree_size(EX(func))`.
        if let Some(held) = self.vm.pop().and_then(|state| state.op_array) {
            if let Some(cache) = held.run_time_cache {
                cache.free(self);
            }
            held.copy.free(self);
        }
        self.stack.pop_frame(&mut self.heap);
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
    ///
    /// php writes an opline's `TMP` with `ZVAL_COPY_VALUE` and never
    /// releases what stood there, because a live temporary is never
    /// written over: the handler that reads one frees it or moves it out.
    /// Here the write hands the old value back to be released, so a `TMP`
    /// whose owner a handler moved without emptying the slot is released
    /// a second time by the next write (#723). A debug build stops that
    /// write: the `TMP` the opline the frame stands at writes, found
    /// holding an owner, is recorded for [`Request::take_sensed`]. A
    /// scalar left there is no owner and no second release, and php
    /// writes over one itself: `JMPZ_EX` and `JMPNZ_EX` write the `TMP`
    /// the `BOOL` after them writes again.
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
        #[cfg(debug_assertions)]
        if old.is_refcounted() {
            self.sense_dirty_temporary(frame, at, old.kind());
        }
        Value::of_zval(old)
    }

    /// A debug build's question at a write over a slot that held an
    /// owner of a `held`: whether the slot is the `TMP` of the opline
    /// `frame` stands at, which the handler that read it last should
    /// have left empty. The first such write is recorded.
    #[cfg(debug_assertions)]
    fn sense_dirty_temporary(&mut self, frame: FrameRef, at: u32, held: php_heap::ZvalType) {
        if self.dirty.is_some() {
            return;
        }
        let opline = self.vm.opline(frame);
        let Some(serving) = self.vm.serving(frame) else {
            return;
        };
        let Some(op) = serving
            .code()
            .and_then(|code| exec::opline_at(code, opline).map(|op| (code, *op)))
        else {
            return;
        };
        let (code, op) = op;
        let its_own = match op.out_type {
            php_unit::OperandKind::Tmp => exec::temporary_slot(code, op.out) == at,
            php_unit::OperandKind::Var
            | php_unit::OperandKind::Cv
            | php_unit::OperandKind::Const
            | php_unit::OperandKind::Unused => false,
        };
        if its_own {
            self.dirty = Some(DirtyTemporary {
                opline,
                opcode: op.opcode,
                number: op.out,
                held,
            });
        }
    }

    /// What a debug build's two sensors found since the loop last asked
    /// (design §10, #761): a `TMP` written over an owner it still held,
    /// or a refcount operation on a block the heap had freed, as the
    /// `InternalError` fatal that ends the request; the first of them,
    /// and taken. The loop asks after every handler.
    #[cfg(debug_assertions)]
    pub(crate) fn take_sensed(&mut self) -> Option<Fatal> {
        let dirty = self.dirty.take().map(|dirty| {
            Fatal::Error(format!(
                "InternalError: {} at opline {} writes its TMP #{} over an owner it still holds ({:?}): the handler that last read the slot moved it out and did not empty it",
                dirty.opcode.name(),
                dirty.opline,
                dirty.number,
                dirty.held,
            ))
        });
        dirty.or_else(|| self.heap.take_freed_use())
    }

    /// The zval in a slot of `frame` in place, php's `EX_VAR(var)`
    /// (design §5.6): what a `FETCH_*_W` points its `VAR` at, and what
    /// the `ASSIGN*` over that `VAR` reads through when an `IS_INDIRECT`
    /// stands there (`_get_zval_ptr_ptr_var`). None for a slot the
    /// frame does not have.
    pub(crate) fn slot_ptr(&mut self, frame: FrameRef, at: u32) -> Option<ZvalRef> {
        self.stack.frame_mut(frame)?.slot_ptr(Slot(at))
    }

    /// A call's or a frame's receiver given up: the owner an owned one
    /// holds released, which may free the object, and the pin a lent one
    /// holds let go, which gives back the bytes of an object freed while
    /// it stood.
    pub(crate) fn give_up_receiver(&mut self, receiver: exec::Receiver) {
        match receiver {
            exec::Receiver::Owned(object) => self.release(object.as_value()),
            exec::Receiver::Lent(object) => self.heap.unpin(object.heap()),
        }
    }

    /// A lent receiver made an owned one, what a frame that leaves the
    /// running chain does with what it was lent: one owner taken, then
    /// the pin let go.
    pub(crate) fn own_receiver(&mut self, object: PhpObject) {
        self.addref(&object.as_value());
        self.heap.unpin(object.heap());
    }

    /// Every lent receiver of the running execution made an owned one,
    /// what a fiber switch does to the execution it leaves.
    pub(crate) fn own_lent_receivers(&mut self) {
        for held in self.vm.own_lent() {
            self.own_receiver(held);
        }
    }

    /// The served call off the stack, `zend_vm_stack_free_args` over what
    /// the `SEND_*` oplines placed and `zend_vm_stack_free_call_frame`
    /// after it. A native's argument is given up as php's `nogc` gives it
    /// up and is no possible root of the cycle collector, where a user
    /// function's, a CV of its frame, is one when the frame is left.
    ///
    /// ```php
    /// $o = new stdClass;
    /// var_dump(is_object($o), gc_status()["roots"]);
    /// function f($x) {}
    /// f($o);
    /// var_dump(gc_status()["roots"]);
    /// ```
    /// ```expect
    /// bool(true)
    /// int(0)
    /// int(1)
    /// ```
    pub(crate) fn pop_call(&mut self) {
        if let Some(call) = self.vm.pop_call() {
            // `zend_vm_stack_free_args`, whose `zval_ptr_dtor_nogc` makes
            // no argument a possible root; the receiver and the closure
            // after them are `OBJ_RELEASE`, which does.
            for value in call.coerced {
                self.release_nogc(value);
            }
            for value in call.arguments {
                self.release_nogc(value);
            }
            if let Some(name) = call.name {
                self.release(Value::string(name));
            }
            if let Some(receiver) = call.receiver {
                self.give_up_receiver(receiver);
            }
            if let Some(closure) = call.closure {
                self.release(closure.as_value());
            }
            self.release_named(call.extra_named);
        }
    }

    /// `zend_free_extra_named_params`: each name and value of a call's
    /// or a frame's extra named arguments given up.
    pub(crate) fn release_named(&mut self, named: exec::ExtraNamed) {
        for (name, value) in named.free(self) {
            self.release(Value::string(name));
            self.release(value);
        }
    }

    /// `zend_get_executed_scope` (`Z/Zend/zend_execute_API.c:712`): the
    /// chain walked from `EG(current_execute_data)` to the first link
    /// that is user code or a function with a scope, and that link's
    /// `func->common.scope`: a user method's class, a closure's bound
    /// scope, and none for a plain user function or the main op array.
    /// A native with no scope, `constant()` or `get_class()` called
    /// from a method, is walked past, so the scope a native reads is
    /// its caller's; a native method's is its own class.
    /// `get_fake_or_executed_scope`: the scope a property is placed and
    /// its visibility measured from, the scope a write named for itself
    /// ([`Request::write_property_as_declared`]) or the executed one.
    pub(crate) fn property_scope(&self) -> Option<ClassId> {
        self.fake_scope.or_else(|| self.executed_scope())
    }

    /// `zend_update_property_ex(prop_info->ce, ...)`, what an engine or a
    /// native write names a property by: the write made in the scope of
    /// the class whose declaration the object's table holds for `name`,
    /// so a private property of a parent class, `Exception::$trace` on a
    /// `TypeError`, is its slot and not a dynamic property; a name no
    /// class declared is written as the executed scope writes it.
    ///
    /// ```php
    /// class C { public $p { get { return intdiv(1, 0); } } }
    /// try { (new C)->p; } catch (DivisionByZeroError $e) { var_dump(count($e->getTrace()), array_key_exists("\0Error\0trace", (array) $e)); }
    /// ```
    /// ```expect
    /// int(2)
    /// bool(true)
    /// ```
    pub(crate) fn write_property_as_declared(
        &mut self,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        let table = self.object_handlers(object.heap());
        let bytes = name.as_bytes().to_vec();
        self.in_declared_scope(object.class(), &bytes, |req| {
            table.write_property(req, object, name, value)
        })
    }

    /// `zend_update_property_ex(scope, ...)`: the write through the
    /// object's `write_property` with `EG(fake_scope)` set to `scope`,
    /// `None` leaving the executed scope to decide, and put back after.
    pub(crate) fn write_property_in(
        &mut self,
        object: PhpObject,
        scope: Option<ClassId>,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        let table = self.object_handlers(object.heap());
        let saved = std::mem::replace(&mut self.fake_scope, scope);
        let done = table.write_property(self, object, name, value);
        self.fake_scope = saved;
        done
    }

    /// `f` called with `EG(fake_scope)` the class whose declaration
    /// `class`'s table holds for `name`, as `zend_update_property_ex(ce,
    /// ...)` makes a write, and put back after it; a name no class
    /// declared leaves the scope as it stands.
    pub(crate) fn in_declared_scope<R>(
        &mut self,
        class: ClassId,
        name: &[u8],
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let declaring = self
            .placed_properties(class)
            .iter()
            .rev()
            .find(|(_, p)| &*p.name.0 == name)
            .map(|(declaring, _)| *declaring);
        let saved = self.fake_scope;
        self.fake_scope = declaring.or(saved);
        let done = f(self);
        self.fake_scope = saved;
        done
    }

    pub(crate) fn executed_scope(&self) -> Option<ClassId> {
        for link in self.chain() {
            let (function, closure, carried) = match link {
                Link::Frame(_, state) => (state.function, state.closure, state.scope),
                Link::Call(call) => (
                    Some(call.function),
                    call.closure,
                    self.is_parent_hook_trampoline(call.function)
                        .then_some(call.called_scope)
                        .flatten(),
                ),
                Link::Site(_) => return None,
            };
            let scope = self
                .closure_scope(closure)
                .or_else(|| self.method_class(function?))
                .or(carried);
            let user = function.is_some_and(|function| self.user_function(function).is_some());
            if scope.is_some() || user || matches!(link, Link::Frame(..)) {
                return scope;
            }
        }
        None
    }

    /// An interned string, never refcounted, for a name the engine holds
    /// as bytes, as `zend_string_init_interned_request` answers it: the
    /// engine's permanent one where a declaration made the name
    /// ([`crate::engine::Shared::permanent`]), which costs the request
    /// nothing, else this request's heap's; the `Err` is the memory
    /// limit refusing the intern.
    ///
    /// A class's and a method's names are `ce->name` and
    /// `function_name`, so only the array of names is counted:
    ///
    /// ```php
    /// $o = new ArrayObject; $i = new ArrayIterator;
    /// $m = memory_get_usage();
    /// $a = get_class($o); $b = $o::class; $c = get_parent_class($i); $d = get_class_methods('Countable');
    /// var_dump(memory_get_usage() - $m);
    /// ```
    /// ```expect
    /// int(216)
    /// ```
    pub(crate) fn interned(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        if let Some(permanent) = self.shared.permanent(bytes) {
            return Ok(PhpString::of(permanent));
        }
        let interned = self.heap.intern(bytes);
        self.fatal_of(interned).map(PhpString::of)
    }

    /// `EG(fake_scope) = class` for the length of `f`, and the scope
    /// that stood put back after it, so it cannot leak past the call:
    /// the scope a native asks the engine to measure visibility from as
    /// though `class` were executing, as `ReflectionClass::newInstance`
    /// asks the object's `get_constructor` (`Z/ext/reflection/php_reflection.c:5086-5088`)
    /// and `zend_update_property_ex` writes. `ReflectionClass::newInstance`
    /// on a class with a private constructor is the witness, in
    /// `ext/reflection`.
    pub fn with_fake_scope<R>(&mut self, class: ClassId, f: impl FnOnce(&mut Request) -> R) -> R {
        let saved = self.fake_scope.replace(class);
        let out = f(self);
        self.fake_scope = saved;
        out
    }

    /// `zend_objects_new` over `class` (`object_init_ex` without the
    /// constructor): a store entry out of this request's heap with one
    /// owner, a slot per declared instance property, as
    /// `zend_object_properties_size` counts them, and the memory limit
    /// as the fatal that ends the request.
    pub(crate) fn new_object(&mut self, class: ClassId) -> Result<PhpObject, Thrown> {
        let slots = self.class_slots(class);
        let defaults = self.property_defaults(class)?;
        // `ce->create_object`: a class that declares a native payload
        // lays it down here, with the object, as
        // `zend_object_alloc(sizeof(custom), ce)` does; every other
        // class is `zend_objects_new` (design §5.4).
        let custom = self.object_custom(class);
        let allocated = match self.native_new_of(class) {
            Some(new) => new(&mut self.heap, class.0, slots, custom),
            None => self.heap.object(class.0, slots, custom),
        };
        let object = match self.fatal_of(allocated).map(PhpObject::of) {
            Ok(object) => object,
            Err(thrown) => {
                self.release_defaults(defaults);
                return Err(thrown);
            }
        };
        self.init_properties(object, defaults);
        self.object_created(object)?;
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
        let custom = self.object_custom(class);
        self.new_object_custom(class, payload, custom)
    }

    /// [`Request::new_object_with`] for an object php `emalloc`s whole at
    /// `size` bytes rather than at its class's `zend_object_alloc`: an
    /// iterator the engine's wrapper stands for, whose struct is the
    /// `get_iterator` that made it's ([`crate::interfaces`]).
    pub(crate) fn new_object_sized<T: std::any::Any + Send>(
        &mut self,
        class: ClassId,
        payload: T,
        size: usize,
    ) -> Result<PhpObject, Thrown> {
        const HEADER: usize = 40;
        self.new_object_custom(class, payload, size.saturating_sub(HEADER))
    }

    fn new_object_custom<T: std::any::Any + Send>(
        &mut self,
        class: ClassId,
        payload: T,
        custom: usize,
    ) -> Result<PhpObject, Thrown> {
        let slots = self.class_slots(class);
        let defaults = self.property_defaults(class)?;
        let allocated = self.heap.object_with(class.0, slots, custom, payload);
        let object = match self.fatal_of(allocated).map(PhpObject::of) {
            Ok(object) => object,
            Err(thrown) => {
                self.release_defaults(defaults);
                return Err(thrown);
            }
        };
        self.init_properties(object, defaults);
        self.object_created(object)?;
        Ok(object)
    }

    /// `object_properties_init`: each declared instance property's slot
    /// takes its default, `zend_update_class_constants` having evaluated
    /// the expression it was written as in the scope of the class that
    /// declared it (`ce->properties_info_table[i]->ce`); a property with
    /// no default is `null` where it is untyped and stays uninitialised
    /// where it is typed, as `zend_declare_typed_property` leaves it.
    /// The slots are [`Request::placed_properties`]', so an inherited
    /// property takes the parent's default in the parent's slot.
    ///
    /// The defaults are evaluated before the object is made, as
    /// `zend_update_class_constants` runs inside `object_init_ex` before
    /// `zend_objects_new`, so an object a default makes, an enum's case
    /// among them, takes its number in the object store before the object being made does.
    ///
    /// ```php
    /// class A { public $p = E::V; }
    /// enum E { case V; }
    /// var_dump(new A);
    /// ```
    /// ```expect
    /// object(A)#2 (1) {
    ///   ["p"]=>
    ///   enum(E::V)
    /// }
    /// ```
    fn property_defaults(&mut self, class: ClassId) -> Result<Vec<Option<Value>>, Thrown> {
        self.default_properties(class)
    }

    /// Defaults [`Request::property_defaults`] evaluated for an object
    /// that was never made, given up.
    fn release_defaults(&mut self, defaults: Vec<Option<Value>>) {
        for value in defaults.into_iter().flatten() {
            self.release(value);
        }
    }

    /// The defaults written into the object's slots, each slot's owner
    /// from here; a typed property with none stays uninitialised.
    fn init_properties(&mut self, object: PhpObject, defaults: Vec<Option<Value>>) {
        for (i, value) in defaults.into_iter().enumerate() {
            let slot = u32::try_from(i).unwrap_or(u32::MAX);
            // A typed slot with no default is `IS_UNDEF` and
            // `IS_PROP_UNINIT`, as `object_properties_init` copies it.
            let Some(value) = value else {
                object
                    .heap()
                    .set_slot_bit(slot, php_heap::PropBit::Uninit, true);
                continue;
            };
            Heap::object_set_slot(&mut Hooks(self), object.heap(), slot, value.into_zval());
        }
    }

    /// `ce->default_properties_table` beside `ce->properties_info_table`
    /// as `zend_do_inheritance` and `zend_build_properties_info_table`
    /// leave them (`Z/Zend/zend_inheritance.c:1889`, `:1686`): the
    /// declared instance properties in slot order, a parent's before the
    /// class's own, since inheritance copies the parent's table to the
    /// front of the child's and moves the child's own up behind it. A
    /// property the class redeclares keeps the slot the parent gave it,
    /// `do_inherit_property`'s `child_info->offset =
    /// parent_info->offset`, and the class's own declaration is what
    /// stands there. A parent's *private* property is not redeclarable:
    /// php leaves its slot alone and gives the child's a slot of its
    /// own, which is how `var_dump` shows both. Each row names the class
    /// that declared what stands in the slot, which is the scope a
    /// private name is measured against, `property_info->ce`.
    ///
    /// The order is what `rebuild_object_properties_internal` walks
    /// (`Z/Zend/zend_object_handlers.c:75`), so it is the order
    /// `var_dump`, the `(array)` cast and a `foreach` over an object
    /// read; `ReflectionMethod` declares `$class` over
    /// `ReflectionFunctionAbstract`'s `$name`, so the inherited name
    /// prints first. An inherited property has a slot from here on, so
    /// it takes the parent's default where php's
    /// `object_properties_init` copies one.
    ///
    /// ```php
    /// class P { public $a = 1; private $p = 2; protected $q = 3; }
    /// class C extends P { public $b = 4; public $p = 5; }
    /// var_dump(new C, new ReflectionMethod('Exception', 'getCode'), (new ReflectionClass('JsonException'))->newInstanceWithoutConstructor()->getMessage());
    /// ```
    /// ```expect
    /// object(C)#1 (5) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["p":"P":private]=>
    ///   int(2)
    ///   ["q":protected]=>
    ///   int(3)
    ///   ["b"]=>
    ///   int(4)
    ///   ["p"]=>
    ///   int(5)
    /// }
    /// object(ReflectionMethod)#2 (2) {
    ///   ["name"]=>
    ///   string(7) "getCode"
    ///   ["class"]=>
    ///   string(9) "Exception"
    /// }
    /// string(0) ""
    /// ```
    ///
    /// The `(array)` cast mangles by the slot and not by the name, so
    /// the two `$p` slots are two keys, php's
    /// `zend_mangle_property_name` naming the private one with the class
    /// that declared it:
    ///
    /// ```php
    /// class P { private $p = 1; protected $q = 2; public $r = 3; }
    /// class C extends P { public $p = 9; public $s = 4; }
    /// var_dump((array)(new C));
    /// ```
    /// ```expect
    /// array(5) {
    ///   ["%0P%0p"]=>
    ///   int(1)
    ///   ["%0*%0q"]=>
    ///   int(2)
    ///   ["r"]=>
    ///   int(3)
    ///   ["p"]=>
    ///   int(9)
    ///   ["s"]=>
    ///   int(4)
    /// }
    /// ```
    ///
    /// The table is built once per class, at the first read, and read
    /// from the class's row after, as php builds it once at the link
    /// (`zend_build_properties_info_table`): what a linked class
    /// declares, and the chain above it, never changes.
    pub(crate) fn placed_properties(&self, class: ClassId) -> &[(ClassId, PropertyDecl)] {
        let row = usize::try_from(class.0).ok().and_then(|at| {
            match at.checked_sub(self.shared.classes.len()) {
                None => self.shared.placed.get(at),
                Some(declared) => self.declared.get(declared).map(|row| &row.placed),
            }
        });
        match row {
            Some(row) => row.get_or_init(|| self.place_properties(class)),
            None => &[],
        }
    }

    /// [`Request::placed_properties`] built from the declarations.
    fn place_properties(&self, class: ClassId) -> Placed {
        let mut chain = Vec::new();
        let mut at = Some(class);
        for _ in 0..64 {
            let Some(here) = at else { break };
            chain.push(here);
            at = self.class_parent(here);
        }
        // Each row beside whether it is still `ZEND_ACC_VIRTUAL`, which
        // a virtual property is while no declaration of it up the chain
        // is backed, and which has no slot (`ZEND_VIRTUAL_PROPERTY_OFFSET`):
        // a backed redeclaration of a virtual one takes a slot of its
        // own where the child's properties are, as
        // `do_inherit_property` leaves the child's offset alone over a
        // virtual parent.
        let mut table: Vec<(ClassId, PropertyDecl, bool)> = Vec::new();
        for here in chain.into_iter().rev() {
            let Some(decl) = self.class_decl(here) else {
                continue;
            };
            for property in decl.properties.iter().filter(|p| p.is_instance) {
                let declared_virtual = property.is_virtual;
                let inherited = table.iter().position(|(_, p, _)| {
                    p.name == property.name && p.visibility != Visibility::Private
                });
                match inherited {
                    Some(i) if !table[i].2 => table[i] = (here, property.clone(), false),
                    Some(i) => {
                        table.remove(i);
                        table.push((here, property.clone(), declared_virtual));
                    }
                    None => table.push((here, property.clone(), declared_virtual)),
                }
            }
        }
        table
            .into_iter()
            .filter(|(_, _, is_virtual)| !is_virtual)
            .map(|(class, property, _)| (class, property))
            .collect()
    }

    /// [`Request::new_object`] by the class's name, what `object_init_ex`
    /// is over a class entry an extension holds. php's caller cannot
    /// name a class nothing registered, since it hands over the entry
    /// itself; here a name the bundle does not link is the
    /// `InternalError` fatal that says which, and no object is made.
    pub(crate) fn new_object_of(&mut self, class_name: &str) -> Result<PhpObject, Thrown> {
        let Some(class) = self.class_named(class_name.as_bytes()) else {
            return Err(crate::api::raising::fatal(
                self,
                &format!("InternalError: the class {class_name} is not linked into this engine"),
            ));
        };
        self.new_object(class)
    }

    /// `ce->default_properties_count`: the declared instance properties
    /// of the class and of every class above it, each a slot on the
    /// object ([`Request::placed_properties`]).
    fn class_slots(&self, class: ClassId) -> u32 {
        u32::try_from(self.placed_properties(class).len()).unwrap_or(u32::MAX)
    }

    /// The slot a declared instance property of `class` sits in,
    /// `zend_get_property_offset` without visibility yet; `None` for a
    /// name no class of the chain declares, which is a dynamic property.
    /// The last row of the name is the one `ce->properties_info` holds,
    /// since a class's own declaration is appended after a parent's
    /// private one of the same name and hides it.
    fn prop_slot(&self, class: ClassId, name: &[u8]) -> Option<u32> {
        self.placed_properties(class)
            .iter()
            .rposition(|(_, p)| &*p.name.0 == name)
            .and_then(|i| u32::try_from(i).ok())
    }

    /// The dynamic table's entry for `name`, a view; none where the
    /// object has no table or the table no such key.
    pub(crate) fn dynamic_prop(o: Obj, name: &[u8]) -> Option<Value> {
        o.properties()?
            .iter()
            .find(|(k, _)| matches!(k, php_heap::Key::Str(s) if s.bytes() == name))
            .map(|(_, z)| Value::of_zval(z))
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

    /// `zend_get_property_offset` over the placed instance properties:
    /// the slot `name` sits in, the class that declared what stands
    /// there, its visibility, its set visibility and whether it is
    /// typed; none for a name no class of the chain declares, which is
    /// a dynamic property. The row is the one the executed scope finds
    /// ([`Request::placed_row`]). A declaration that shadows a parent's
    /// private property of the name (`ZEND_ACC_CHANGED`) is, from that
    /// parent's scope, the parent's own private slot
    /// (`zend_get_parent_private_property`).
    ///
    /// ```php
    /// class A { private int $prop = 42;
    ///     function r() { return $this->prop; }
    ///     function op() { $this->prop += 1; return $this->prop; }
    ///     function ref() { $x =& $this->prop; unset($this->prop); $x = "foo"; var_dump($x); } }
    /// class B extends A { private $prop = "b"; }
    /// $b = new B;
    /// var_dump($b->r(), $b->op());
    /// $b->ref();
    /// ```
    /// ```expect
    /// int(42)
    /// int(43)
    /// string(3) "foo"
    /// ```
    fn declared_slot(&self, class: ClassId, name: &[u8]) -> Option<DeclaredSlot> {
        let (at, declaring, p) = self.placed_row(class, name)?;
        let root = if p.visibility == Visibility::Protected {
            self.property_root(declaring, name)
        } else {
            declaring
        };
        Some(DeclaredSlot {
            slot: u32::try_from(at).unwrap_or(u32::MAX),
            declaring,
            visibility: p.visibility,
            set_visibility: p.set_visibility,
            typed: p.ty.is_some(),
            readonly: p.is_readonly,
            root,
        })
    }

    /// `property_info->prototype->ce` for the instance property `name`
    /// that `declaring` declares: `do_inherit_property` hands a child
    /// the parent's prototype where the parent's declaration is not
    /// private, so the walk goes up the parents while each one that
    /// declares the name declares it so.
    fn property_root(&self, declaring: ClassId, name: &[u8]) -> ClassId {
        let mut root = declaring;
        let mut at = self.class_parent(declaring);
        for _ in 0..64 {
            let Some(here) = at else { break };
            let own = self.class_decl(here).and_then(|decl| {
                decl.properties
                    .iter()
                    .find(|p| p.is_instance && &*p.name.0 == name)
                    .map(|p| p.visibility)
            });
            match own {
                Some(Visibility::Private) => break,
                Some(_) => root = here,
                None => {}
            }
            at = self.class_parent(here);
        }
        root
    }

    /// `zend_get_property_offset`'s access rule over the row
    /// [`Request::declared_slot`] placed: public, the scope the declaring
    /// class, or, for a protected property, a scope related to its
    /// prototype's class (`is_protected_compatible_scope`), so two
    /// classes that each redeclare a parent's property reach each other's.
    ///
    /// ```php
    /// abstract class P { abstract protected $foo { get; } }
    /// class C1 extends P { protected $foo = 1; }
    /// class C2 extends P { protected $foo = 2; static function foo($c) { return $c->foo; } }
    /// var_dump(C2::foo(new C2), C2::foo(new C1));
    /// ```
    /// ```expect
    /// int(2)
    /// int(1)
    /// ```
    fn reaches(&self, placed: &DeclaredSlot) -> bool {
        let scope = self.property_scope();
        match placed.visibility {
            Visibility::Public => true,
            _ if scope == Some(placed.declaring) => true,
            Visibility::Private => false,
            Visibility::Protected => self.check_protected(placed.root, scope),
        }
    }

    /// `zend_get_property_offset` where the row `ce->properties_info`
    /// holds for `name` is a `static` property, which the placed
    /// instance rows never are: `None` where the row is not one, and
    /// otherwise the rule over it, `Err` with its visibility where the
    /// scope may not reach it (`zend_bad_property_access`, the caller's
    /// to raise unless it places silently), and `Ok` where it may, the
    /// notice raised unless `silent` and the name then a dynamic
    /// property. The row is the class's own declaration, else the
    /// nearest parent's, a private one included, and under
    /// `ZEND_ACC_CHANGED` the scope's own private `static` where the
    /// scope is a parent of `class` that declares one.
    ///
    /// ```php
    /// class A { private static $a = "a"; protected static $b = "b"; public static $c = "c";
    ///     function f() { var_dump($this->a, $this->b, $this->c); } }
    /// class B extends A {}
    /// (new B)->f();
    /// try { var_dump((new B)->b); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    ///
    /// Notice: Accessing static property B::$a as non static in %s on line 3
    ///
    /// Warning: Undefined property: B::$a in %s on line 3
    ///
    /// Notice: Accessing static property B::$b as non static in %s on line 3
    ///
    /// Warning: Undefined property: B::$b in %s on line 3
    ///
    /// Notice: Accessing static property B::$c as non static in %s on line 3
    ///
    /// Warning: Undefined property: B::$c in %s on line 3
    /// NULL
    /// NULL
    /// NULL
    /// Cannot access protected property B::$b
    /// ```
    pub(crate) fn static_as_instance(
        &mut self,
        class: ClassId,
        name: &[u8],
        silent: bool,
    ) -> Option<Result<(), Visibility>> {
        let mut at = Some(class);
        let mut row = None;
        for _ in 0..64 {
            let Some(here) = at else { break };
            if let Some(p) = self
                .class_decl(here)
                .and_then(|decl| decl.properties.iter().find(|p| &*p.name.0 == name))
            {
                row = Some((here, p.is_instance, p.visibility));
                break;
            }
            at = self.class_parent(here);
        }
        let (declaring, is_instance, visibility) = row?;
        let scope = self.property_scope();
        if scope != Some(declaring) {
            // `ZEND_ACC_CHANGED`: the scope's own private `static` wins
            // where the row is a `static` too.
            let own = scope.is_some_and(|scope| {
                scope != class
                    && self.instance_of(class, scope)
                    && self.class_decl(scope).is_some_and(|decl| {
                        decl.properties.iter().any(|p| {
                            !p.is_instance
                                && p.visibility == Visibility::Private
                                && &*p.name.0 == name
                        })
                    })
            });
            match visibility {
                _ if is_instance => return None,
                _ if own => {}
                Visibility::Public => {}
                Visibility::Private if declaring != class => return None,
                Visibility::Protected if self.check_protected(declaring, scope) => {}
                Visibility::Private | Visibility::Protected => return Some(Err(visibility)),
            }
        } else if is_instance {
            return None;
        }
        if !silent {
            let shown = self.class_name(class);
            crate::api::raising::notice(
                self,
                &format!(
                    "Accessing static property {shown}::${} as non static",
                    String::from_utf8_lossy(name)
                ),
            );
        }
        Some(Ok(()))
    }

    /// `ZEND_WRONG_PROPERTY_OFFSET` for a name no instance row placed:
    /// a name led by a NUL byte, or a `static` property the scope may
    /// not reach ([`Request::static_as_instance`], whose notice for one
    /// it may reach is raised here unless `silent`). `None` is a
    /// dynamic property.
    fn unplaced(&mut self, class: ClassId, name: &[u8], silent: bool) -> Option<Unplaced> {
        if Self::bad_property_name(name) {
            return Some(Unplaced::BadName);
        }
        match self.static_as_instance(class, name, silent) {
            Some(Err(visibility)) => Some(Unplaced::Denied(visibility)),
            _ => None,
        }
    }

    /// The `Error` for what [`Request::unplaced`] refused.
    fn unplaced_error(&mut self, class: ClassId, name: &[u8], miss: Unplaced) -> Thrown {
        match miss {
            Unplaced::BadName => self.bad_property_name_error(),
            Unplaced::Denied(visibility) => self.inaccessible_property(class, visibility, name),
        }
    }

    /// `zend_get_property_offset`'s choice of `property_info` for `name`
    /// on an object of `class` under the executed scope, as the placed
    /// row it names: the row `ce->properties_info` holds, the class's
    /// own declaration over a parent's private one; under
    /// `ZEND_ACC_CHANGED` the scope's own private property where the
    /// scope is a parent of `class` that declares one
    /// (`zend_get_parent_private_property`); and none where the row is
    /// a parent's private property the scope is not, which php reads as
    /// a dynamic property (`goto dynamic`).
    ///
    /// ```php
    /// class A { private $shadowed = 'A'; private $p = 'A::p';
    ///     function test() { foreach ($this as $k => $v) { echo "$k => $v\n"; } var_dump($this->p); } }
    /// #[AllowDynamicProperties]
    /// class B extends A { public $p = 'B::p'; }
    /// $b = new B;
    /// $b->shadowed = 'Global';
    /// $b->test();
    /// var_dump($b->p);
    /// ```
    /// ```expect
    /// shadowed => A
    /// p => A::p
    /// string(4) "A::p"
    /// string(4) "B::p"
    /// ```
    /// `zend_get_parent_private_property`: the executed scope, where it
    /// is a parent of `class` that declares `name` as a private instance
    /// property of its own.
    pub(crate) fn parent_private(&self, class: ClassId, name: &[u8]) -> Option<ClassId> {
        self.property_scope().filter(|&scope| {
            scope != class
                && self.instance_of(class, scope)
                && self.class_decl(scope).is_some_and(|decl| {
                    decl.properties.iter().any(|p| {
                        p.is_instance && p.visibility == Visibility::Private && &*p.name.0 == name
                    })
                })
        })
    }

    fn placed_row(&self, class: ClassId, name: &[u8]) -> Option<(usize, ClassId, PropertyDecl)> {
        let placed = self.placed_properties(class);
        let mut at = placed.iter().rposition(|(_, p)| &*p.name.0 == name)?;
        let (declaring, p) = placed.get(at)?;
        if self.property_scope() != Some(*declaring) {
            if let Some(own) = self.parent_private(class, name) {
                if let Some(i) = placed
                    .iter()
                    .position(|(d, p)| *d == own && &*p.name.0 == name)
                {
                    at = i;
                }
            } else if p.visibility == Visibility::Private && *declaring != class {
                return None;
            }
        }
        let (declaring, p) = placed.get(at)?;
        Some((at, *declaring, p.clone()))
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
        let scope = self.property_scope();
        scope == Some(class) || (set == Visibility::Protected && self.check_protected(class, scope))
    }

    /// Whether `property`, which `declaring` declares, has a set
    /// visibility the executed scope lacks.
    pub(crate) fn lacks_set_access(&self, declaring: ClassId, property: &PropertyDecl) -> bool {
        property
            .set_visibility
            .is_some_and(|set| !self.has_set_access(declaring, set))
    }

    /// `prop_info->flags & ZEND_ACC_PPP_SET_MASK &&
    /// !zend_asymmetric_property_has_set_access(prop_info)`, the test the
    /// opcodes over a `static` property make of it once it is fetched,
    /// with php's `Error` for `operation`: `modify` for
    /// `ASSIGN_STATIC_PROP` and `indirectly modify` for the fetches and
    /// the read-modify-write opcodes.
    ///
    /// ```php
    /// class C { public private(set) static int $p = 1; public private(set) static array $a = [];
    ///     static function set() { self::$p = 5; self::$a[] = 1; } }
    /// try { C::$p = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { C::$p++; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { C::$p .= "1"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { C::$a[] = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $r = &C::$p; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// C::set();
    /// var_dump(C::$p, C::$a);
    /// ```
    /// ```expect
    /// Cannot modify private(set) property C::$p from global scope
    /// Cannot indirectly modify private(set) property C::$p from global scope
    /// Cannot indirectly modify private(set) property C::$p from global scope
    /// Cannot indirectly modify private(set) property C::$a from global scope
    /// Cannot indirectly modify private(set) property C::$p from global scope
    /// int(5)
    /// array(1) {
    ///   [0]=>
    ///   int(1)
    /// }
    /// ```
    pub(crate) fn set_refused(
        &mut self,
        declaring: ClassId,
        property: &PropertyDecl,
        operation: &str,
    ) -> Option<Thrown> {
        let set = property.set_visibility?;
        if !self.lacks_set_access(declaring, property) {
            return None;
        }
        Some(self.set_inaccessible_property(
            declaring,
            set,
            property.is_readonly,
            &property.name.0,
            operation,
        ))
    }

    /// `zend_asymmetric_visibility_property_modification_error`
    /// (`Z/Zend/zend_execute.c:931`): php's `Error` for a write the set
    /// visibility refuses, `operation` the word the site does
    /// (`modify`, `indirectly modify`, `unset`). php names the called
    /// scope here, not the executed one; the two differ only where a
    /// method is reached through a class that inherited it, and the
    /// linker that inherits a method arrives with the classes of design
    /// §11 days 8-14. A readonly property's `protected(set)` is named
    /// with its `readonly`, which is the set visibility
    /// `zend_declare_typed_property` gives every public readonly one:
    ///
    /// ```php
    /// class A { public readonly int $x; function __construct(bool $set) { if ($set) { $this->x = 1; } } }
    /// $a = new A(false);
    /// try { $a->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { unset($a->x); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $b = new A(true);
    /// try { $b->x = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { unset($b->x); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot modify protected(set) readonly property A::$x from global scope
    /// Cannot unset protected(set) readonly property A::$x from global scope
    /// Cannot modify readonly property A::$x
    /// Cannot unset readonly property A::$x
    /// ```
    ///
    /// `__clone` may unset and set one again, php's `IS_PROP_REINITABLE`:
    ///
    /// ```php
    /// class F { function __construct(public readonly stdClass $b) {} function __clone() { unset($this->b); $this->b = new stdClass; } }
    /// $f = new F(new stdClass);
    /// $g = clone $f;
    /// var_dump($f->b === $g->b);
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    fn set_inaccessible_property(
        &mut self,
        class: ClassId,
        set: Visibility,
        readonly: bool,
        name: &[u8],
        operation: &str,
    ) -> Thrown {
        let word = match set {
            Visibility::Private => "private(set)",
            _ if readonly => "protected(set) readonly",
            _ => "protected(set)",
        };
        let from = match self.executed_scope() {
            Some(scope) => format!("scope {}", self.class_name(scope)),
            None => String::from("global scope"),
        };
        let class = self.class_name(class);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Cannot {operation} {word} property {class}::${shown} from {from}"),
        )
    }

    /// `zend_verify_property_access`: whether the executed scope may
    /// reach a property of `class` declared with `visibility`.
    fn may_access(&self, class: ClassId, visibility: Visibility) -> bool {
        self.may_access_from(class, visibility, self.executed_scope())
    }

    /// [`Request::may_access`] from a scope named rather than the one
    /// executing: what `zend_verify_const_access` checks a class
    /// constant against inside a constant expression, the scope it is
    /// evaluated in.
    fn may_access_from(
        &self,
        class: ClassId,
        visibility: Visibility,
        scope: Option<ClassId>,
    ) -> bool {
        match visibility {
            Visibility::Public => true,
            _ if scope == Some(class) => true,
            Visibility::Private => false,
            Visibility::Protected => self.check_protected(class, scope),
        }
    }

    /// `zend_hash_find_ptr(&ce->properties_info, name)` over the table
    /// `zend_do_inheritance` left: the class's own declaration of
    /// `name`, then each parent's, since a parent's entries are
    /// appended into the child's `properties_info` and one of the same
    /// name the child declares hides the parent's. The answer names the
    /// class that declared it, which for a `static` property is the
    /// class whose slot the value sits in (php's `IS_INDIRECT` into the
    /// parent's table) and the scope its visibility is measured
    /// against. An instance property is found here too, as php finds
    /// one in the same table and rejects it after the access check.
    pub(crate) fn property_info_of(
        &self,
        class: ClassId,
        name: &[u8],
    ) -> Option<(ClassId, PropertyDecl)> {
        let mut at = Some(class);
        for _ in 0..64 {
            let here = at?;
            let decl = self.class_decl(here)?;
            if let Some(property) = decl.properties.iter().find(|p| &*p.name.0 == name) {
                return Some((here, property.clone()));
            }
            at = self.class_parent(here);
        }
        None
    }

    /// `ZEND_REF_ADD_TYPE_SOURCE` (`Z/Zend/zend_types.h:1082`): the
    /// declared type of a property whose slot holds `cell` is put on the
    /// cell, so every write through any name bound to it is held to the
    /// type as well ([`Request::assign_to_reference`]). A cell typed by
    /// nothing yet takes a row of its own, whose word the cell carries.
    pub(crate) fn ref_add_type_source(&mut self, cell: Ref, source: RefSource) {
        let word = cell.sources();
        if word != 0
            && let Some((_, sources)) = self.ref_sources.iter_mut().find(|(at, _)| *at == word)
        {
            sources.push(source);
            return;
        }
        let word = self.next_ref_source;
        self.next_ref_source = self.next_ref_source.saturating_add(1);
        self.ref_sources.push((word, vec![source]));
        cell.set_sources(word);
    }

    /// `ZEND_REF_DEL_TYPE_SOURCE`: the property is no longer one of the
    /// cell's sources, because its slot now holds something else. The
    /// row goes with the last source, and the cell's word with it.
    pub(crate) fn ref_del_type_source(&mut self, cell: Ref, declaring: ClassId, name: &[u8]) {
        let word = cell.sources();
        if word == 0 {
            return;
        }
        let Some(at) = self.ref_sources.iter().position(|(held, _)| *held == word) else {
            return;
        };
        if let Some((_, sources)) = self.ref_sources.get_mut(at) {
            if let Some(i) = sources
                .iter()
                .position(|s| s.declaring == declaring && s.name == name)
            {
                sources.remove(i);
            }
            if !sources.is_empty() {
                return;
            }
        }
        self.ref_sources.remove(at);
        cell.set_sources(0);
    }

    /// `ZEND_REF_DEL_TYPE_SOURCE` over the property whose slot `slot`
    /// of `object` is (`zend_get_property_info_for_slot`): the cell the
    /// slot held is typed by that property no longer, because the slot
    /// is unset, reset or freed. A slot of an untyped property typed
    /// nothing.
    pub(crate) fn slot_source_gone(&mut self, object: PhpObject, slot: u32, cell: Ref) {
        let typed = usize::try_from(slot)
            .ok()
            .and_then(|at| self.placed_properties(object.class()).get(at))
            .filter(|(_, p)| p.ty.is_some())
            .map(|(declaring, p)| (*declaring, p.name.0.clone()));
        if let Some((declaring, name)) = typed {
            self.ref_del_type_source(cell, declaring, &name);
        }
    }

    /// `zend_object_dtor_property` then `ZVAL_UNDEF`: what the slot held
    /// given up, a cell it held typed by the slot's property no longer
    /// ([`Request::slot_source_gone`]), and the slot left empty. What a
    /// lazy object does to its slots goes through here: a reset, the
    /// proxy's slots once its instance stands, and an initializer's
    /// revert, which types the cell it puts back again.
    ///
    /// ```php
    /// class C { public ?C $a; public $c; public function __construct() { $this->a = null; } }
    /// $r = new ReflectionClass(C::class);
    /// $objs = ['ghost' => $r->newLazyGhost(function ($o) { $o->__construct(); }),
    ///     'proxy' => $r->newLazyProxy(fn () => new C),
    ///     'failed' => $r->newLazyGhost(function ($o) { throw new Exception('no'); })];
    /// foreach ($objs as $k => $obj) {
    ///     $r->getProperty('a')->setRawValueWithoutLazyInitialization($obj, null);
    ///     $ref = &$obj->a;
    ///     try { $obj->c; } catch (Exception $e) { echo "$k: ", $e->getMessage(), "\n"; }
    ///     try { $ref = 1; echo "$k: bound to nothing typed\n"; } catch (TypeError $e) { echo "$k: ", $e->getMessage(), "\n"; }
    ///     unset($ref);
    /// }
    /// ```
    /// ```expect
    /// ghost: Cannot assign int to reference held by property C::$a of type ?C
    /// proxy: bound to nothing typed
    /// failed: no
    /// failed: Cannot assign int to reference held by property C::$a of type ?C
    /// ```
    pub(crate) fn dtor_property(&mut self, object: PhpObject, slot: u32) {
        let o = object.heap();
        if let Some(cell) = o.slot(slot).as_reference() {
            self.slot_source_gone(object, slot, cell);
        }
        if !o.slot(slot).is_undef() {
            Heap::object_set_slot(&mut Hooks(self), o, slot, Zval::UNDEF);
        }
    }

    /// `zend_handle_fetch_obj_flags` under `ZEND_FETCH_REF`
    /// (`Z/Zend/zend_execute.c:3470`) over a typed property's slot `at`:
    /// a slot nothing has written refused where the type takes no `null`
    /// ([`Request::refuse_unwritten_ref`]), and a slot holding no cell yet
    /// made one, `null` where nothing was written, with the property as
    /// its type source, so every write through a name bound to it is held
    /// to the type ([`Request::assign_to_reference`]). `FETCH_OBJ_W`'s
    /// handler takes the place so for `&$o->p`, and
    /// [`PhpObject::typed_row_ref`] for a row of the property table.
    pub(crate) fn fetch_typed_slot_ref(
        &mut self,
        at: php_heap::ZvalRef,
        typed: &TypedProperty,
        name: &[u8],
    ) -> Result<(), Thrown> {
        let held = Value::of_zval(at.read());
        self.refuse_unwritten_ref(&held, typed, name)?;
        if held.as_php_ref().is_none() {
            let cell = self.reference(held.undef_as_null())?;
            at.write(cell.as_value().to_zval());
            let source = RefSource {
                declaring: typed.declaring,
                name: name.to_vec(),
                ty: typed.ty.clone(),
            };
            self.ref_add_type_source(cell.heap(), source);
        }
        Ok(())
    }

    /// `ZEND_FETCH_REF`'s refusal (`zend_handle_fetch_obj_flags`): a
    /// typed property nothing has written, `held` its slot, bound by `&`
    /// where its type takes no `null`, which php's `Error` names.
    pub(crate) fn refuse_unwritten_ref(
        &mut self,
        held: &Value,
        typed: &TypedProperty,
        name: &[u8],
    ) -> Result<(), Thrown> {
        if held.tag() != TypeTag::Undef || typed.ty.mask() & php_unit::decl::Type::Null.mask() != 0
        {
            return Ok(());
        }
        let class = self.class_name(typed.declaring);
        let shown = String::from_utf8_lossy(name);
        Err(crate::api::raising::throw_error(
            self,
            "Error",
            &format!(
                "Cannot access uninitialized non-nullable property {class}::${shown} by reference"
            ),
        ))
    }

    /// `zend_get_typed_property_info_for_slot`: the typed property whose
    /// slot `slot` of an object of `class` is, beside its unmangled name;
    /// none for an untyped one.
    pub(crate) fn slot_typed_property(
        &self,
        class: ClassId,
        slot: u32,
    ) -> Option<(TypedProperty, Vec<u8>)> {
        let placed = self.placed_properties(class);
        let at = usize::try_from(slot).ok()?;
        let (declaring, p) = placed.get(at)?;
        let name = p.name.0.to_vec();
        let typed = TypedProperty {
            declaring: *declaring,
            ty: p.ty.clone()?,
            readonly: p.is_readonly,
        };
        Some((typed, name))
    }

    /// `ZEND_REF_ADD_TYPE_SOURCE` over the property whose slot `slot` of
    /// `object` is (`zend_get_property_info_for_slot`): the cell the slot
    /// holds is typed by that property too, one more source beside the
    /// others, since another object's slot of the same property may
    /// hold it as well. A slot of an untyped property types nothing.
    pub(crate) fn slot_source_added(&mut self, object: PhpObject, slot: u32, cell: Ref) {
        let placed = self.placed_properties(object.class());
        let Some((declaring, p)) = usize::try_from(slot).ok().and_then(|at| placed.get(at)) else {
            return;
        };
        if let Some(ty) = p.ty.clone() {
            let source = RefSource {
                declaring: *declaring,
                name: p.name.0.to_vec(),
                ty,
            };
            self.ref_add_type_source(cell, source);
        }
    }

    /// `ZEND_REF_FOREACH_TYPE_SOURCES` as a slice: the properties whose
    /// slots hold this cell, empty for a cell no typed property's slot
    /// holds, which is `ZEND_REF_HAS_TYPE_SOURCES` reading false.
    pub(crate) fn ref_type_sources(&self, cell: Ref) -> &[RefSource] {
        let word = cell.sources();
        if word == 0 {
            return &[];
        }
        self.ref_sources
            .iter()
            .find(|(at, _)| *at == word)
            .map_or(&[], |(_, sources)| sources.as_slice())
    }

    /// `zend_assign_to_typed_ref` (`Z/Zend/zend_execute.c:4070`): a
    /// write through a cell, held first to every type its sources
    /// declare ([`crate::ops::verify_ref_assignable`]) and coerced to
    /// what they agree on. A cell no typed property's slot holds is
    /// [`Request::reference_set`] and nothing else, which is what php's
    /// `zend_assign_to_variable` does where `ZEND_REF_HAS_TYPE_SOURCES`
    /// reads false. The value is given up with the `TypeError` and what
    /// the cell held stands, as php gives up its copy.
    pub(crate) fn assign_to_reference(
        &mut self,
        cell: Ref,
        value: Value,
        strict: bool,
    ) -> Result<(), Thrown> {
        if cell.sources() == 0 {
            self.reference_set(cell, value);
            return Ok(());
        }
        match crate::ops::verify_ref_assignable(self, cell, &value, strict) {
            Ok(None) => self.reference_set(cell, value),
            Ok(Some(coerced)) => {
                self.release(value);
                self.reference_set(cell, coerced);
            }
            Err(thrown) => {
                self.release(value);
                return Err(thrown);
            }
        }
        Ok(())
    }

    /// php's notice where a write fetch fell back to `__get` and what it
    /// answered is neither an object nor a reference: a write through the
    /// copy has no effect on anything (`zend_std_read_property`, `Indirect
    /// modification of overloaded property`), naming the object's class.
    ///
    /// A private property the scope may not reach is `__get`'s too (#513):
    /// an int it answers is the notice, and an object is written through.
    ///
    /// ```php
    /// class C { private $p = 1; private $q;
    ///     function __construct() { $this->q = new stdClass; }
    ///     function __get($n) { return $this->$n; } }
    /// $c = new C;
    /// try { $c->p->x = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $c->q->x = 3;
    /// var_dump($c);
    /// ```
    /// ```expect
    ///
    /// Notice: Indirect modification of overloaded property C::$p has no effect in %s on line 6
    /// Attempt to assign property "x" on int
    /// object(C)#1 (2) {
    ///   ["p":"C":private]=>
    ///   int(1)
    ///   ["q":"C":private]=>
    ///   object(stdClass)#2 (1) {
    ///     ["x"]=>
    ///     int(3)
    ///   }
    /// }
    /// ```
    fn overloaded_write_notice(
        &mut self,
        class: ClassId,
        name: &[u8],
        read: PropertyRead,
        got: &Value,
    ) {
        if matches!(read, PropertyRead::Write | PropertyRead::Unset)
            && got.as_php_ref().is_none()
            && got.as_object().is_none()
        {
            let class = self.class_name(class);
            let shown = String::from_utf8_lossy(name).into_owned();
            crate::api::raising::notice(
                self,
                &format!(
                    "Indirect modification of overloaded property {class}::${shown} has no effect"
                ),
            );
        }
    }

    /// Whether a write fetch over `name` falls back to `read_property`
    /// for the class's `__get` to answer, where php's
    /// `zend_std_get_property_ptr_ptr` answers `NULL` for it: a declared
    /// property the scope may not reach, a declared slot it reaches that
    /// an `unset` emptied, or a name the
    /// object holds no dynamic property for, on a class with a `__get`
    /// not already serving the name. A slot that is `IS_PROP_UNINIT` or
    /// waits on a lazy initializer is the placement's.
    ///
    /// ```php
    /// class M { private $data = ['a' => [1], 'o' => null];
    ///     function __construct() { $this->data['o'] = new stdClass; }
    ///     function __get($n) { echo "__get $n\n"; return $this->data[$n]; } }
    /// $m = new M;
    /// $m->a[] = 2;
    /// $m->o->x = 3;
    /// var_dump($m->a, $m->o);
    /// ```
    /// ```expect
    /// __get a
    ///
    /// Notice: Indirect modification of overloaded property M::$a has no effect in %s on line 6
    /// __get o
    /// __get a
    /// __get o
    /// array(1) {
    ///   [0]=>
    ///   int(1)
    /// }
    /// object(stdClass)#2 (1) {
    ///   ["x"]=>
    ///   int(3)
    /// }
    /// ```
    pub(crate) fn overloaded_fetch(&self, object: PhpObject, name: &[u8]) -> bool {
        let class = object.class();
        if !self.serves_accessor(class, b"__get") {
            return false;
        }
        if self.wrong_name(class, name) {
            return true;
        }
        if self.guard_word(object, name) & IN_GET != 0 {
            return false;
        }
        let o = object.heap();
        match self.declared_slot(class, name) {
            // A property the scope may not reach is placed silently where
            // the class has a `__get`, which answers for it.
            Some(placed) if !self.reaches(&placed) => true,
            Some(placed) => {
                o.slot(placed.slot).is_undef()
                    && !o.slot_uninit(placed.slot)
                    && !self.lazy_slot(o, placed.slot)
            }
            None => !o.properties().is_some_and(|table| {
                table
                    .iter()
                    .any(|(k, _)| matches!(k, php_heap::Key::Str(s) if s.bytes() == name))
            }),
        }
    }

    /// `zend_std_read_property`'s `/* magic isset */` arm under
    /// `BP_VAR_IS`: the `__isset` asked where its guard is down, `null`
    /// where it said no, and the `__get` served after a yes where its
    /// guard is down, what it answered held to the property's type
    /// ([`Request::hold_magic_get`]). `None` is neither accessor
    /// answering, and the read goes on to the warning's arm. The caller
    /// holds the object across the two.
    ///
    /// ```php
    /// class Test {
    ///     public function __isset($x) { $GLOBALS["obj"] = 24; return true; }
    ///     public function __get($x) { var_dump($this); return 42; }
    /// }
    /// $obj = new Test;
    /// $name = "foo";
    /// var_dump($obj->$name ?? 12);
    /// var_dump($obj);
    /// ```
    /// ```expect
    /// object(Test)#1 (0) {
    /// }
    /// int(42)
    /// int(24)
    /// ```
    fn isset_then_get(
        &mut self,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
        issetter: Function,
        getter: Option<Function>,
    ) -> Result<Option<Value>, Thrown> {
        if self.guard_word(object, name) & IN_ISSET == 0 {
            let answered = self.call_accessor(object, issetter, IN_ISSET, name, &[])?;
            let truthy = crate::api::coerce::truthy(self, &answered);
            self.release(answered);
            if !truthy {
                return Ok(Some(Value::null()));
            }
        }
        let Some(getter) = getter.filter(|_| self.guard_word(object, name) & IN_GET == 0) else {
            return Ok(None);
        };
        let class = object.class();
        let got = self.call_accessor(object, getter, IN_GET, name, &[])?;
        self.overloaded_write_notice(class, name, read, &got);
        self.hold_magic_get(class, name, getter, got).map(Some)
    }

    /// `zend_verify_prop_assignable_by_ref_ex` under
    /// `ZEND_VERIFY_PROP_ASSIGNABLE_BY_REF_CONTEXT_MAGIC_GET`: what `__get`
    /// answered for a typed property an `unset` emptied, held to the
    /// property's type under `__get`'s own `strict_types`, converted by the
    /// weak rule where it can be, and php's `TypeError` naming `__get`
    /// where not. A name that is no typed declared property the scope
    /// reaches is the answer as it is.
    ///
    /// ```php
    /// class T { public int $i = 1; public int $u;
    ///     function __get($n) { echo "__get $n\n"; return $n === 'i' ? "42" : "x"; }
    ///     function __isset($n) { echo "__isset $n\n"; return true; } }
    /// $t = new T; unset($t->i);
    /// var_dump($t->i, isset($t->u));
    /// unset($t->u);
    /// try { var_dump($t->u); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// __get i
    /// int(42)
    /// bool(false)
    /// __get u
    /// Value of type string returned from T::__get() must be compatible with unset property T::$u of type int
    /// ```
    ///
    /// A reference `&__get` answers is converted in its cell, so the
    /// property it names reads the conversion too, and a write fetch
    /// binds to that cell:
    ///
    /// ```php
    /// class Test { public $prop = "42"; public int $val;
    ///     public function &__get($name) { return $this->prop; } }
    /// $test = new Test; unset($test->val);
    /// $val = &$test->val;
    /// var_dump($test->prop);
    /// $test->prop = "x";
    /// var_dump($val);
    /// ```
    /// ```expect
    /// int(42)
    /// string(1) "x"
    /// ```
    fn hold_magic_get(
        &mut self,
        class: ClassId,
        name: &[u8],
        getter: Function,
        got: Value,
    ) -> Result<Value, Thrown> {
        let Some(placed) = self
            .declared_slot(class, name)
            .filter(|p| p.typed && self.reaches(p))
        else {
            return Ok(got);
        };
        let Some(typed) = self.declared_typed_property(class, name) else {
            return Ok(got);
        };
        let strict = self.function_strict_types(getter);
        let held = got.deref();
        let cell = got.as_php_ref().copied();
        // A cell some typed property is already a source of converts
        // nothing: it holds the type as it is, or it is refused.
        if let Some(bound) = cell.filter(|cell| cell.heap().sources() != 0) {
            let holds = crate::ops::typed_ref_holds(
                self,
                bound.heap(),
                &typed.ty,
                placed.declaring,
                name,
                strict,
            );
            return match holds {
                Ok(true) => Ok(got),
                Ok(false) => {
                    self.release(got);
                    Err(self.magic_get_refused(placed.declaring, name, &typed.ty, &held))
                }
                Err(thrown) => {
                    self.release(got);
                    Err(thrown)
                }
            };
        }
        let admitted = match crate::ops::property_type_admits(
            self,
            &held,
            &typed.ty,
            placed.declaring,
            strict,
        ) {
            Ok(admitted) => admitted,
            Err(thrown) => {
                self.release(got);
                return Err(thrown);
            }
        };
        match admitted {
            crate::ops::Admitted::As => Ok(got),
            // `i_zend_check_property_type` converts the value where it
            // stands, which for a reference `__get` answered is in the
            // cell, so every name bound to it reads the conversion.
            crate::ops::Admitted::Converted(coerced) => {
                if let Some(cell) = cell {
                    self.reference_set(cell.heap(), coerced);
                    Ok(got)
                } else {
                    self.release(got);
                    Ok(coerced)
                }
            }
            crate::ops::Admitted::Refused => {
                self.release(got);
                Err(self.magic_get_refused(placed.declaring, name, &typed.ty, &held))
            }
        }
    }

    /// `zend_magic_get_property_type_inconsistency_error`: php's
    /// `TypeError` for what `__get` answered for an unset typed property
    /// whose type holds it in no form.
    fn magic_get_refused(
        &mut self,
        declaring: ClassId,
        name: &[u8],
        ty: &php_unit::decl::Type,
        held: &Value,
    ) -> Thrown {
        let given = crate::ops::type_name(self, held);
        let class = self.class_name(declaring);
        let ty = crate::api::coerce::declared_type_name(ty);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw_error(
            self,
            "TypeError",
            &format!(
                "Value of type {given} returned from {class}::__get() must be compatible with unset property {class}::${shown} of type {ty}"
            ),
        )
    }

    /// `zend_readonly_property_modification_error` (`Cannot modify
    /// readonly property C::$p`) and
    /// `zend_readonly_property_indirect_modification_error` (`Cannot
    /// indirectly modify ...`), by `operation`, naming the class that
    /// declared the property.
    ///
    /// Refused from the class's own scope as from any other, the unwritten
    /// property included where the write is indirect; `__clone` writes it
    /// once more, never indirectly:
    ///
    /// ```php
    /// class T {
    ///     public readonly int $i; public readonly array $a;
    ///     function __construct() { $this->i = 1; }
    ///     function bump() { try { $this->i++; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    ///         try { $this->i += 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    ///         try { $this->a[] = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    ///         try { unset($this->i); } catch (Error $e) { echo $e->getMessage(), "\n"; } }
    ///     function __clone() { $this->i = 5;
    ///         try { $this->i = 6; } catch (Error $e) { echo $e->getMessage(), "\n"; } }
    /// }
    /// $t = new T; $t->bump();
    /// var_dump((clone $t)->i);
    /// ```
    /// ```expect
    /// Cannot modify readonly property T::$i
    /// Cannot modify readonly property T::$i
    /// Cannot indirectly modify readonly property T::$a
    /// Cannot unset readonly property T::$i
    /// Cannot modify readonly property T::$i
    /// int(5)
    /// ```
    fn readonly_modification(
        &mut self,
        declaring: ClassId,
        name: &[u8],
        operation: &str,
    ) -> Thrown {
        let class = self.class_name(declaring);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Cannot {operation} readonly property {class}::${shown}"),
        )
    }

    /// php's `Error` for a typed property read before anything wrote it
    /// (`zend_throw_error` under `uninit_error`), naming the class that
    /// declared it, `prop_info->ce`.
    fn uninitialized_property(&mut self, declaring: ClassId, name: &[u8]) -> Thrown {
        let class = self.class_name(declaring);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Typed property {class}::${shown} must not be accessed before initialization"),
        )
    }

    /// `zend_get_property_offset`'s other wrong offset: a name no class
    /// declared that starts with a NUL byte, which is how a mangled
    /// private or protected name reads, is no property PHP code may
    /// reach, and is no dynamic one.
    pub(crate) fn bad_property_name(name: &[u8]) -> bool {
        name.first() == Some(&0)
    }

    /// [`Request::bad_property_name`] over a name `class` did not
    /// declare, which is the wrong offset `zend_get_property_offset`
    /// answers: a `__get` is asked for it whatever its guard says, and
    /// the guard standing is [`Request::wrong_offset`]'s `Error`.
    fn wrong_name(&self, class: ClassId, name: &[u8]) -> bool {
        Self::bad_property_name(name) && self.declared_slot(class, name).is_none()
    }

    /// `zend_bad_property_name`: php's `Error` for a name
    /// [`Request::bad_property_name`] refuses.
    ///
    /// ```php
    /// class A { function __unset($p) { unset($this->$p); }
    ///     function __set($p, $v) { $this->$p = $v; }
    ///     function __get($p) { return $this->$p; } }
    /// $a = new A; $p = "\0x"; $o = new stdClass;
    /// foreach ([fn() => $a->$p, fn() => $a->$p = 1, function () use ($a, $p) { unset($a->$p); },
    ///           fn() => $o->$p, fn() => $o->$p = 1, fn() => $o->$p[] = 1, fn() => $o->$p++] as $f) {
    ///     try { $f(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// var_dump(isset($o->$p), isset($a->$p), property_exists($o, $p));
    /// ```
    /// ```expect
    /// Cannot access property starting with "\0"
    /// Cannot access property starting with "\0"
    /// Cannot access property starting with "\0"
    /// Cannot access property starting with "\0"
    /// Cannot access property starting with "\0"
    /// Cannot access property starting with "\0"
    /// Cannot access property starting with "\0"
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// ```
    pub(crate) fn bad_property_name_error(&mut self) -> Thrown {
        crate::api::raising::throw_error(
            self,
            "Error",
            "Cannot access property starting with \"\\0\"",
        )
    }

    /// `zend_wrong_offset`: the placement a magic accessor's guard
    /// kept silent made again, loudly, where the guard stands and the
    /// accessor cannot answer: the `Error` for a declared property the
    /// scope may not reach, or for a name that starts with a NUL byte.
    fn wrong_offset(&mut self, class: ClassId, name: &[u8]) -> Option<Thrown> {
        if let Some(placed) = self.declared_slot(class, name) {
            return (!self.reaches(&placed))
                .then(|| self.inaccessible_property(class, placed.visibility, name));
        }
        let miss = self.unplaced(class, name, true)?;
        Some(self.unplaced_error(class, name, miss))
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
        crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Cannot access {word} property {class}::${shown}"),
        )
    }

    /// `zobj->ce->__get` and its three neighbours: the magic accessor
    /// `lowered` names, which php resolves once at link time and holds
    /// on the class entry as the pointer every handler tests. Each of
    /// the four is public and takes a receiver, which
    /// `zend_check_magic_method_implementation` refuses a class that
    /// declares otherwise, so the scope that reached the property never
    /// bars the call.
    fn magic_accessor(&self, class: ClassId, lowered: &[u8]) -> Option<Function> {
        self.method_named(class, lowered)
    }

    /// `zobj->ce->__set != NULL`, the test a handler makes before it
    /// places a name: whether the class serves the magic accessor
    /// `lowered` names, which is what makes a placement silent
    /// ([`Request::declared_slot_for_write`]).
    pub(crate) fn serves_accessor(&self, class: ClassId, lowered: &[u8]) -> bool {
        self.magic_accessor(class, lowered).is_some()
    }

    /// `ce->ce_flags & ZEND_ACC_USE_GUARDS`: whether `class` or a class
    /// above it declares `__get`, `__set`, `__isset`, `__unset` or
    /// `__debugInfo` (`zend_add_magic_method` sets the bit for each,
    /// and `zend_do_inheritance` passes it down), or implements an
    /// interface that hands the bit on
    /// ([`php_unit::decl::ClassFlags::use_guards`]).
    pub(crate) fn uses_guards(&self, class: ClassId) -> bool {
        const MAGIC: [&[u8]; 5] = [b"__get", b"__set", b"__isset", b"__unset", b"__debuginfo"];
        MAGIC
            .iter()
            .any(|name| self.method_named(class, name).is_some())
            || self.guards_handed_on(class, 0)
    }

    /// Whether `class`, a parent of it or an interface of either carries
    /// [`php_unit::decl::ClassFlags::use_guards`]; a chain longer than any
    /// declaration can make is cut, as [`Request::instance_of`] cuts it.
    fn guards_handed_on(&self, class: ClassId, depth: u32) -> bool {
        let Some(decl) = self.class_decl(class) else {
            return false;
        };
        if decl.flags.use_guards {
            return true;
        }
        if depth > 64 {
            return false;
        }
        decl.interfaces
            .iter()
            .chain(decl.parent.as_ref())
            .filter_map(|name| self.class_named(&name.0))
            .any(|above| self.guards_handed_on(above, depth + 1))
    }

    /// [`crate::api::values::PhpObject::is_recursive_in`].
    pub(crate) fn recursion_guarded(
        &self,
        object: PhpObject,
        walk: crate::api::values::RecursionGuard,
    ) -> bool {
        if !self.uses_guards(object.class()) {
            return object.is_recursive();
        }
        let id = object.heap().id();
        self.recursion_guards.contains(&(id, walk))
    }

    /// [`crate::api::values::PhpObject::protect_in`].
    pub(crate) fn recursion_protect(
        &mut self,
        object: PhpObject,
        walk: crate::api::values::RecursionGuard,
    ) {
        if !self.uses_guards(object.class()) {
            object.protect();
            return;
        }
        let row = (object.heap().id(), walk);
        if !self.recursion_guards.contains(&row) {
            self.recursion_guards.push(row);
        }
    }

    /// [`crate::api::values::PhpObject::unprotect_in`].
    pub(crate) fn recursion_unprotect(
        &mut self,
        object: PhpObject,
        walk: crate::api::values::RecursionGuard,
    ) {
        if !self.uses_guards(object.class()) {
            object.unprotect();
            return;
        }
        let row = (object.heap().id(), walk);
        self.recursion_guards.retain(|held| *held != row);
    }

    /// `*zend_get_property_guard(zobj, name)`: the word `name` carries
    /// on `object`, zero for a name no accessor is serving.
    fn guard_word(&self, object: PhpObject, name: &[u8]) -> u32 {
        let id = object.heap().id();
        self.guards
            .iter()
            .find(|((held, under), _)| *held == id && under == name)
            .map_or(0, |(_, word)| *word)
    }

    /// `(*guard) |= bit`, the row laid down by the first accessor to
    /// take it.
    fn guard_take(&mut self, object: PhpObject, name: &[u8], bit: u32) {
        let id = object.heap().id();
        if let Some((_, word)) = self
            .guards
            .iter_mut()
            .find(|((held, under), _)| *held == id && under == name)
        {
            *word |= bit;
            return;
        }
        self.guards.push(((id, name.to_vec()), bit));
    }

    /// `(*guard) &= ~bit`, and the row dropped once the word is empty.
    /// php frees the word with the object; a row here is gone before
    /// the accessor that took it returns, so an `spl_object_id()` the
    /// store hands out again names no row of an object that has died.
    fn guard_drop(&mut self, object: PhpObject, name: &[u8], bit: u32) {
        let id = object.heap().id();
        let at = self
            .guards
            .iter()
            .position(|((held, under), _)| *held == id && under == name);
        let Some(at) = at else {
            return;
        };
        let Some((_, word)) = self.guards.get_mut(at) else {
            return;
        };
        *word &= !bit;
        if *word == 0 {
            self.guards.swap_remove(at);
        }
    }

    /// `zend_std_call_getter` and its three neighbours
    /// (`Z/Zend/zend_object_handlers.c:836`): one magic accessor served
    /// on the object with the property's name as its first argument and
    /// `arguments` after it, under `bit` in the property guard, which is
    /// taken before the call and given back after it whatever the call
    /// did. The object is one more owner for the span of the call
    /// (`GC_ADDREF`, `OBJ_RELEASE`), so an accessor that drops the last
    /// other owner does not free the object under the handler.
    fn call_accessor(
        &mut self,
        object: PhpObject,
        accessor: Function,
        bit: u32,
        name: &[u8],
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        self.guard_take(object, name, bit);
        let served = self.serve_accessor(object, accessor, name, arguments);
        self.guard_drop(object, name, bit);
        served
    }

    /// [`Request::call_accessor`] without the guard, for the one site
    /// that holds a bit across two calls: `zend_std_has_property` keeps
    /// `IN_ISSET` standing while it asks the `__get` that `empty` reads
    /// the truth of.
    fn serve_accessor(
        &mut self,
        object: PhpObject,
        accessor: Function,
        name: &[u8],
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        let interned = self.interned(name)?;
        let mut sent = Vec::with_capacity(arguments.len().saturating_add(1));
        sent.push(Value::string(interned));
        sent.extend_from_slice(arguments);
        let held = self.take_hold(object.as_value());
        let served = self.serve_method(accessor, Some(object), object.class(), &sent, &[]);
        self.release_hold(held);
        served
    }

    /// `zend_std_read_property`, the handler `FETCH_OBJ_R` and
    /// `FETCH_OBJ_IS` call: `zend_get_property_offset` places the name,
    /// a declared property's slot where the scope may see it, php's
    /// `Error` `Cannot access private property C::$p` where it may not
    /// (`zend_bad_property_access`), and the dynamic table for a name
    /// the class did not declare. A typed slot nothing has written is
    /// php's `Error` for a property read before initialization; an
    /// untyped one that `unset` emptied reads as a dynamic name does,
    /// and a name neither holds warns `Undefined property: C::$p` and
    /// is `null`. What is read is one more owner of the value
    /// (`ZVAL_COPY_DEREF`), a reference followed.
    ///
    /// A class with a `__get` is served between the slot and the
    /// warning, under `IN_GET` in the property guard: the placement is
    /// silent where a `__get` may serve what it refuses
    /// (`zend_get_property_offset(ce, name, (type == BP_VAR_IS) ||
    /// (ce->__get != NULL), ...)`), so a property the scope may not
    /// reach is the wrong-offset marker and reaches the accessor rather
    /// than the `Error`; the uninitialised typed slot is the one arm
    /// that skips `__get`, php's `IS_PROP_UNINIT`; and a `__get` that
    /// reads the name it was called for finds the guard standing and
    /// falls through to the warning, with the `Error` raised again
    /// (`zend_wrong_offset`) where the placement was the one the scope
    /// may not reach.
    ///
    /// Under [`PropertyRead::Silent`], php's `BP_VAR_IS`, each of the
    /// three is `null` instead: `zend_get_property_offset` returns the
    /// wrong-offset marker rather than raising for a property the scope
    /// may not reach, the uninitialized typed slot is `ZVAL_NULL`, and
    /// the warning is the one line php guards with `type != BP_VAR_IS`.
    /// A class with a `__isset` serves it there before the `__get`, and
    /// a falsy answer is the `null` with no `__get` called at all.
    ///
    /// ```php
    /// class M { public function __get($k) { echo "get $k\n"; return 7; } }
    /// $m = new M;
    /// var_dump($m->z);
    /// class P { private $secret = 5; public function __get($k) { echo "get $k\n"; return 9; } }
    /// $p = new P;
    /// var_dump($p->secret);
    /// class T { public int $t; public function __get($k) { echo "get $k\n"; return 1; } }
    /// $t = new T;
    /// try { var_dump($t->t); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// get z
    /// int(7)
    /// get secret
    /// int(9)
    /// Error: Typed property T::$t must not be accessed before initialization
    /// ```
    ///
    /// The guard: a `__get` that reads the name it was called for finds
    /// it standing and warns where the recursion would have been.
    ///
    /// ```php
    /// class R { public function __get($k) { echo "in get $k\n"; return $this->a; } }
    /// $r = new R;
    /// var_dump($r->a);
    /// ```
    /// ```expect
    /// in get a
    ///
    /// Warning: Undefined property: R::$a in %s on line 2
    /// NULL
    /// ```
    ///
    /// A lazy object is initialised where the read finds nothing
    /// (`zend_lazy_object_init` at `uninit_error`): a declared slot that
    /// waits skips `__get` and initialises the object, while a name no
    /// class declared is the accessor's first.
    ///
    /// ```php
    /// class C { public $a = 1; public function __get($n) { echo "get $n\n"; return 0; } }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function ($o) { echo "init\n"; });
    /// var_dump($o->dyn);
    /// var_dump($r->isUninitializedLazyObject($o));
    /// var_dump($o->a);
    /// ```
    /// ```expect
    /// get dyn
    /// int(0)
    /// bool(true)
    /// init
    /// int(1)
    /// ```
    pub(crate) fn read_property(
        &mut self,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        let silent = read == PropertyRead::Silent;
        // What nothing holds is `null`, quietly, under `BP_VAR_IS` and
        // `BP_VAR_UNSET`; only the first asks `__isset`.
        let quiet = matches!(read, PropertyRead::Silent | PropertyRead::Unset);
        let o = object.heap();
        let class = object.class();
        let shown = String::from_utf8_lossy(name).into_owned();
        if let Some(read) = self.read_hooked_arm(object, name, read) {
            return read;
        }
        // `ZEND_WRONG_PROPERTY_OFFSET`: the placement found a property
        // the scope may not reach and said so quietly, because a
        // `__get` may serve it. php tests one pointer for that; the
        // class is asked for the accessor only where the answer is
        // read, so a slot that holds a value is placed and returned
        // without a lookup.
        let mut wrong = false;
        if let Some(placed) = self.declared_slot(class, name) {
            if self.reaches(&placed) {
                let z = o.slot(placed.slot);
                if !z.is_undef() {
                    let value = Value::of_zval(z).deref();
                    self.addref(&value);
                    return Ok(value);
                }
                // `IS_PROP_UNINIT | IS_PROP_LAZY`: a slot that waits for
                // the initializer skips `__get` too, and the object is
                // initialised and read instead (`uninit_error`).
                if self.lazy_slot(o, placed.slot) {
                    let instance = self.lazy_init(object)?;
                    return self.read_property(instance, name, read);
                }
                // `IS_PROP_UNINIT`: a typed slot nothing has written
                // skips `__get` and is the uninitialised-access error; one
                // an `unset` emptied is the magic methods' where the class
                // has one that answers, and the same error where not
                // (`uninit_error` with `prop_info` set).
                let magic = self.serves_accessor(class, b"__get")
                    || (silent && self.serves_accessor(class, b"__isset"));
                if placed.typed && (o.slot_uninit(placed.slot) || !magic) {
                    if quiet {
                        return Ok(Value::null());
                    }
                    return Err(self.uninitialized_property(placed.declaring, name));
                }
            } else {
                if !silent && !self.serves_accessor(class, b"__get") {
                    return Err(self.inaccessible_property(class, placed.visibility, name));
                }
                wrong = true;
            }
        } else if let Some(miss) =
            self.unplaced(class, name, silent || self.serves_accessor(class, b"__get"))
        {
            if !silent && !self.serves_accessor(class, b"__get") {
                return Err(self.unplaced_error(class, name, miss));
            }
            wrong = true;
        } else {
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
        }
        let getter = self.magic_accessor(class, b"__get");
        let issetter = silent
            .then(|| self.magic_accessor(class, b"__isset"))
            .flatten();
        let guard_bit = if issetter.is_some() { IN_ISSET } else { IN_GET };
        // GH-21478: a proxy that forwards, read from inside the
        // instance's own accessor for this name, reads the instance
        // directly rather than serving the accessor twice.
        if let Some(instance) = self.lazy_instance(o)
            && self.guard_word(instance, name) & guard_bit != 0
        {
            return self.read_property(instance, name, read);
        }
        // `/* magic isset */`: under `BP_VAR_IS` a class with a
        // `__isset` is asked first, and the `__get` after it only where
        // it said yes, the object held across both (`GC_ADDREF(zobj)`
        // before the `__isset`, `OBJ_RELEASE` after the `__get`), so
        // an `__isset` that gives up every other owner does not free
        // the object the `__get` is served on.
        if let Some(issetter) = issetter {
            let held = self.take_hold(object.as_value());
            let served = self.isset_then_get(object, name, read, issetter, getter);
            self.release_hold(held);
            if let Some(served) = served? {
                return Ok(served);
            }
        } else if let Some(getter) = getter {
            if self.guard_word(object, name) & IN_GET == 0 {
                let got = self.call_accessor(object, getter, IN_GET, name, &[])?;
                self.overloaded_write_notice(class, name, read, &got);
                return self.hold_magic_get(class, name, getter, got);
            }
            // `zend_wrong_offset`: the guard stands and the placement
            // was a wrong offset, so it is made again, loudly, and the
            // `Error` it raises is the answer.
            if wrong && let Some(thrown) = self.wrong_offset(class, name) {
                return Err(thrown);
            }
        }
        // `uninit_error` over a lazy object: a name nothing holds
        // initialises it and is read on what that answers, under the
        // guard the accessor above stood under where one did.
        if o.laziness() != php_heap::Laziness::Eager {
            let instance = self.lazy_init(object)?;
            let guarded = getter.is_some() || issetter.is_some();
            if guarded && self.guard_word(instance, name) & guard_bit == 0 {
                self.guard_take(instance, name, guard_bit);
                let read = self.read_property(instance, name, read);
                self.guard_drop(instance, name, guard_bit);
                return read;
            }
            return self.read_property(instance, name, read);
        }
        if !quiet {
            let class = self.class_name(class);
            crate::api::raising::warning(self, &format!("Undefined property: {class}::${shown}"));
        }
        Ok(Value::null())
    }

    /// `zend_std_write_property`'s `found` arm over a slot that holds a
    /// value: an initialised readonly property is modified from no scope,
    /// save once more under `IS_PROP_REINITABLE`, and a set visibility
    /// the scope lacks refuses the write too; `None` lets it through.
    fn found_refusal(&mut self, o: Obj, p: &DeclaredSlot, name: &[u8]) -> Option<Thrown> {
        if p.readonly && !o.slot_bit(p.slot, php_heap::PropBit::Reinitable) {
            return Some(self.readonly_modification(p.declaring, name, "modify"));
        }
        let set = p.set_visibility?;
        if self.has_set_access(p.declaring, set) {
            return None;
        }
        Some(self.set_inaccessible_property(p.declaring, set, p.readonly, name, "modify"))
    }

    /// Whether `zend_std_get_property_ptr_ptr`
    /// (`Z/Zend/zend_object_handlers.c:1485`) hands out no place for the
    /// declared property `name` the scope reaches, so that a compound
    /// assignment is `read_property`, the operation and `write_property`:
    /// a readonly property, and one whose set visibility the scope lacks.
    /// What the operation throws then comes before what the write
    /// refuses.
    ///
    /// ```php
    /// class Test { public readonly object $prop; public private(set) int $n = 1;
    ///     function __construct() { $this->prop = new stdClass; } }
    /// $t = new Test;
    /// try { $t->prop += 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $t->prop .= "x"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $t->n += 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Unsupported operand types: stdClass + int
    /// Object of class stdClass could not be converted to string
    /// Cannot modify private(set) property Test::$n from global scope
    /// ```
    pub(crate) fn place_withheld(&self, object: PhpObject, name: &[u8]) -> bool {
        let Some(placed) = self.declared_slot(object.class(), name) else {
            return false;
        };
        if !self.may_access(placed.declaring, placed.visibility) {
            return false;
        }
        placed.readonly
            || placed
                .set_visibility
                .is_some_and(|set| !self.has_set_access(placed.declaring, set))
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
    /// `silent` is php's own argument to `zend_get_property_offset`: a
    /// site whose class serves the accessor that may take the write
    /// places the name quietly, so a property the scope may not reach
    /// is `None`, the wrong-offset marker, and the handler goes on to
    /// `write_property` or to the overloaded pair, where the `Error` is
    /// raised again if no accessor takes it.
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
        object: PhpObject,
        name: &[u8],
        operation: &str,
        silent: bool,
    ) -> Result<Option<(u32, bool)>, Thrown> {
        let class = object.class();
        let Some(placed) = self.declared_slot(class, name) else {
            if let Some(miss) = self.unplaced(class, name, silent)
                && !silent
            {
                return Err(self.unplaced_error(class, name, miss));
            }
            return Ok(None);
        };
        if !self.reaches(&placed) {
            if silent {
                return Ok(None);
            }
            return Err(self.inaccessible_property(class, placed.visibility, name));
        }
        // A slot an `unset` emptied, on a class with a `__set`, is the
        // setter's: `zend_std_write_property` reaches the magic method
        // before the set visibility, which `write_std_property` holds the
        // write to after it.
        let o = object.heap();
        if o.slot(placed.slot).is_undef()
            && !o.slot_uninit(placed.slot)
            && !self.lazy_slot(o, placed.slot)
            && self.serves_accessor(class, b"__set")
        {
            return Ok(None);
        }
        // `zend_readonly_property_modification_error` and its indirect
        // twin, ahead of the set visibility: an initialised readonly
        // property is modified from no scope, save once more under
        // `IS_PROP_REINITABLE` inside `__clone`, and an unwritten one is
        // no place for an indirect modification either, which would
        // initialise it by a write the property never sees.
        let initialized = !o.slot(placed.slot).is_undef();
        // `IS_PROP_REINITABLE` lets `__clone` write the property again,
        // never modify it indirectly (`zend_std_read_property` under
        // `BP_VAR_W` does not read the bit).
        let indirect = operation == "indirectly modify";
        if placed.readonly
            && (indirect
                || (initialized && !o.slot_bit(placed.slot, php_heap::PropBit::Reinitable)))
        {
            return Err(self.readonly_modification(placed.declaring, name, operation));
        }
        // The set visibility after the readonly arm, which a readonly
        // property `IS_PROP_REINITABLE` has passed: `__clone` and a
        // `clone with` write it once more only from a scope that may set
        // it (`zend_std_write_property`).
        if let Some(set) = placed.set_visibility
            && !self.has_set_access(placed.declaring, set)
        {
            return Err(self.set_inaccessible_property(
                placed.declaring,
                set,
                placed.readonly,
                name,
                operation,
            ));
        }
        Ok(Some((placed.slot, placed.typed)))
    }

    /// `if (UNEXPECTED(Z_TYPE_P(retval) == IS_UNDEF))` in
    /// `zend_std_get_property_ptr_ptr` under `BP_VAR_RW`, ahead of any
    /// placement: a declared slot the scope reaches that nothing holds,
    /// where no `__get` answers for it or it is `IS_PROP_UNINIT`, raises
    /// the read's own diagnostic, the typed property's `Error` naming the
    /// class that declared it or the warning, an untyped one laid to
    /// `null` before the warning as php lays it. Anything else is the
    /// placement's.
    pub(crate) fn read_before_write(
        &mut self,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        let class = object.class();
        let o = object.heap();
        let Some(placed) = self.declared_slot(class, name) else {
            return Ok(());
        };
        if !self.reaches(&placed)
            || !o.slot(placed.slot).is_undef()
            || self.lazy_slot(o, placed.slot)
        {
            return Ok(());
        }
        let magic =
            self.serves_accessor(class, b"__get") && self.guard_word(object, name) & IN_GET == 0;
        if magic && !o.slot_uninit(placed.slot) {
            return Ok(());
        }
        if placed.typed {
            return Err(self.uninitialized_property(placed.declaring, name));
        }
        // `ZVAL_NULL(retval)` before the warning, so what an error handler
        // writes into the slot is what the fetch then reads.
        self.write_declared_slot(o, placed.slot, Value::null())?;
        let class = self.class_name(class);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::warning(self, &format!("Undefined property: {class}::${shown}"));
        Ok(())
    }

    /// Whether a write fetch over `name` reads the property rather than
    /// placing it: a declared slot the scope reaches whose readonly or
    /// set visibility bars this scope from writing it, holding an object.
    /// php's `zend_std_get_property_ptr_ptr` hands such a slot to
    /// `read_property`, which under `BP_VAR_W` returns the object as a
    /// copy (`zend_std_read_property`), so a write through the object
    /// changes the object and never the property.
    ///
    /// Under an `unset` fetch the same slot holding nothing is read too,
    /// as `null` and quietly: there is nothing to modify.
    pub(crate) fn write_fetch_reads_object(
        &self,
        object: PhpObject,
        name: &[u8],
        unset: bool,
    ) -> bool {
        let Some(placed) = self.declared_slot(object.class(), name) else {
            return false;
        };
        let barred = placed.readonly
            || placed
                .set_visibility
                .is_some_and(|set| !self.has_set_access(placed.declaring, set));
        let held = object.heap().slot(placed.slot);
        barred
            && self.reaches(&placed)
            && (Value::of_zval(held).deref().as_object().is_some() || (unset && held.is_undef()))
    }

    /// The `zend_property_info` beside a typed slot, what
    /// `zend_assign_to_typed_prop` is handed: the declared type, the
    /// class that declared it, and `ZEND_ACC_READONLY`. Read only where
    /// [`Request::declared_slot_for_write`] said the slot is typed, so
    /// an untyped write pays nothing for it; a property php lets carry
    /// `readonly` is typed by php's own refusal, so no untyped write
    /// misses the bit.
    pub(crate) fn declared_typed_property(
        &self,
        class: ClassId,
        name: &[u8],
    ) -> Option<TypedProperty> {
        let (_, declaring, p) = self.placed_row(class, name)?;
        p.ty.map(|ty| TypedProperty {
            declaring,
            ty,
            readonly: p.is_readonly,
        })
    }

    /// [`Request::declared_typed_property`] for a `static` property: the
    /// `zend_property_info` `zend_fetch_static_property_address` hands on,
    /// its type and the class that declared it, where the property is
    /// typed.
    pub(crate) fn declared_typed_static(
        &self,
        class: ClassId,
        name: &[u8],
    ) -> Option<TypedProperty> {
        let (declaring, p) = self.property_info_of(class, name)?;
        if p.is_instance {
            return None;
        }
        p.ty.map(|ty| TypedProperty {
            declaring,
            ty,
            readonly: p.is_readonly,
        })
    }

    /// `zend_verify_property_type` over a value about to land in a typed
    /// property, the step `zend_assign_to_typed_prop`,
    /// `zend_binary_assign_op_typed_prop`, `zend_incdec_typed_prop` and
    /// `zend_std_write_property` share: the value as given where the
    /// declared type accepts it, the weak rule's conversion in its place
    /// where `strict` is off and the rule makes one, and php's
    /// `TypeError` otherwise. `value` is given up on every arm that does
    /// not land it.
    pub(crate) fn hold_to_property_type(
        &mut self,
        typed: &TypedProperty,
        name: &[u8],
        value: Value,
        strict: bool,
    ) -> Result<Value, Thrown> {
        let verified = crate::ops::verify_property_type(
            self,
            &value,
            &typed.ty,
            typed.declaring,
            name,
            strict,
        );
        match verified {
            Ok(None) => Ok(value),
            Ok(Some(coerced)) => {
                self.release(value);
                Ok(coerced)
            }
            Err(thrown) => {
                self.release(value);
                Err(thrown)
            }
        }
    }

    /// `zend_std_write_property`'s `typed_property` arm around
    /// [`Request::hold_to_property_type`]: the object held one owner more
    /// while the value is verified (`GC_ADDREF(zobj)`), since a
    /// `__toString` the weak rule serves may give its last owner up, and
    /// where that hold was the last one, php's `Error` and the object
    /// destroyed after it, the value given up (`GC_DELREF(zobj) == 0`,
    /// `Z/Zend/zend_object_handlers.c:1113`).
    ///
    /// ```php
    /// class A { public string $prop; }
    /// class B { public function __toString() { global $a; $a = null; return "a"; } }
    /// $a = new A();
    /// try { $a->prop = new B(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $a = new A(); $a->prop = '';
    /// try { $a->prop = new B(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($a);
    /// ```
    /// ```expect
    /// Object was released while assigning to property A::$prop
    /// Object was released while assigning to property A::$prop
    /// NULL
    /// ```
    pub(crate) fn hold_to_property_type_of(
        &mut self,
        object: PhpObject,
        typed: &TypedProperty,
        name: &[u8],
        value: Value,
        strict: bool,
    ) -> Result<Value, Thrown> {
        let held = self.take_hold(object.as_value());
        let verified = self.hold_to_property_type(typed, name, value, strict);
        let Ok(landing) = verified else {
            self.let_go(held);
            return verified;
        };
        if object.as_value().refcount() > 1 {
            self.let_go(held);
            return Ok(landing);
        }
        let class = self.class_name(typed.declaring);
        let shown = String::from_utf8_lossy(name).into_owned();
        let error = crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Object was released while assigning to property {class}::${shown}"),
        );
        self.let_go(held);
        self.release(landing);
        Err(error)
    }

    /// `zend_assign_to_variable` over a declared slot, `ASSIGN_OBJ`'s
    /// fast path over a slot that holds a value (`fast_assign_obj`):
    /// `value` is the slot's from here and what it held is released
    /// last, after the write, which is where a destructor is served; a
    /// slot holding a reference is written through the cell
    /// (`Z_ISREF_P(variable_ptr)`), so every holder of the cell reads
    /// the new value. A cell some typed property is a source of is
    /// `zend_assign_to_typed_ref`, held to each of their types under
    /// `strict` ([`Request::assign_to_reference`]), whatever the slot's
    /// own property declares. What landed comes back, a view: the
    /// coercion where the cell's types made one. Beside it comes what the
    /// slot held, one owner, for the caller to release after it has
    /// copied what landed into `result`, as php's `garbage` is given up
    /// last: a destructor that writes the property again does not change
    /// the value the assignment answers.
    ///
    /// ```php
    /// class C { public int $foo = 1; public $_; }
    /// $a = new C; $a->_ = &$a->foo;
    /// try { $a->_ = []; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($a->_ = "5", $a->foo);
    /// ```
    /// ```expect
    /// Cannot assign array to reference held by property C::$foo of type int
    /// int(5)
    /// int(5)
    /// ```
    ///
    /// What the slot held is given up after the assignment's value is
    /// copied, so a destructor it serves that writes the property again
    /// changes the property and not the value:
    ///
    /// ```php
    /// class Box { public ?Test $value; public static ?Test $s; }
    /// class Test { function __destruct() { global $box; $box->value = null; Box::$s = null; } }
    /// $box = new Box; $box->value = new Test; Box::$s = new Test;
    /// var_dump($box->value = new Test, Box::$s = new Test, $box->value, Box::$s);
    /// ```
    /// ```expect
    /// object(Test)#4 (0) {
    /// }
    /// object(Test)#2 (0) {
    /// }
    /// NULL
    /// object(Test)#2 (0) {
    /// }
    /// ```
    pub(crate) fn assign_declared_slot(
        &mut self,
        o: Obj,
        i: u32,
        value: Value,
        strict: bool,
    ) -> Result<(Value, Value), Thrown> {
        if let Some(r) = o.slot(i).as_reference() {
            let cell = PhpRef::of(r);
            let garbage = cell.get();
            self.addref(&garbage);
            if let Err(thrown) = self.assign_to_reference(r, value, strict) {
                self.release(garbage);
                return Err(thrown);
            }
            return Ok((cell.get(), garbage));
        }
        let garbage = Value::of_zval(o.slot(i));
        self.addref(&garbage);
        Heap::object_set_slot(&mut Hooks(self), o, i, value.into_zval());
        Ok((value, garbage))
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

    /// `zend_std_write_property`, the `Std` slot: the placement, then
    /// the class's `__set` for a name it found nothing at, and
    /// otherwise `write_std_property`
    /// ([`Request::write_std_property`]).
    ///
    /// php places the name with `zend_get_property_offset(ce, name,
    /// ce->__set != NULL, ...)` and reaches the setter where the
    /// placement found no value to write over: a name no class declared
    /// that the dynamic table does not hold, a declared untyped slot an
    /// `unset` emptied, and a declared slot the scope may not reach,
    /// which is the wrong-offset marker a `__set` makes quiet. A
    /// declared typed slot nothing has written is the one arm that
    /// bypasses `__set`, and so is every slot that holds a value. The
    /// setter is served under `IN_SET` in the property guard, so a
    /// `__set` writing the name it was called for lays the property
    /// down instead of reaching itself; where the placement was the one
    /// the scope may not reach, that second pass is the `Error`
    /// (`zend_wrong_offset`) rather than a write.
    ///
    /// `value` is given up here whatever arm runs, as php's caller
    /// gives it up, and so is `name`. An initialised readonly slot is
    /// php's `Error`, from the declaring scope too, as Reflection writes:
    ///
    /// ```php
    /// enum Foo: int { case Bar = 0; }
    /// try { (new ReflectionProperty(Foo::class, 'value'))->setValue(Foo::Bar, 1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump(Foo::Bar->value);
    /// ```
    /// ```expect
    /// Cannot modify readonly property Foo::$value
    /// int(0)
    /// ```
    ///
    /// ```php
    /// class M { public $kept = 1; public function __set($k, $v) { echo "set $k=$v\n"; } }
    /// $m = new M;
    /// $m->y = 3;
    /// $m->kept = 2;
    /// var_dump($m);
    /// class P { private $secret = 5; public function __set($k, $v) { echo "set $k=$v\n"; } }
    /// $p = new P;
    /// $p->secret = 8;
    /// var_dump($p);
    /// ```
    /// ```expect
    /// set y=3
    /// object(M)#1 (1) {
    ///   ["kept"]=>
    ///   int(2)
    /// }
    /// set secret=8
    /// object(P)#2 (1) {
    ///   ["secret":"P":private]=>
    ///   int(5)
    /// }
    /// ```
    pub(crate) fn write_property(
        &mut self,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        let class = object.class();
        // `if (Z_TYPE_P(variable_ptr) != IS_UNDEF) goto found` and
        // `if (Z_PROP_FLAG_P(variable_ptr) & IS_PROP_UNINIT) goto
        // write_std_property`: a declared slot the scope may reach that
        // holds a value, and a typed one nothing has written, are both
        // the setter's business to stay out of, so neither asks the
        // class for one.
        let o = object.heap();
        let lazy = o.laziness() != php_heap::Laziness::Eager;
        if let Some(hooked) = self.hooked(class, name.as_bytes()) {
            if self.hooked_accessible(&hooked) {
                return self.write_hooked(object, name, value, hooked);
            }
            if !self.serves_accessor(class, b"__set") {
                let thrown = self.inaccessible_property(class, hooked.visibility, name.as_bytes());
                self.release(value);
                self.release(Value::string(name));
                return Err(thrown);
            }
        }
        let placed = self.declared_slot(class, name.as_bytes());
        if let Some(p) = placed.as_ref()
            && self.reaches(p)
        {
            if !o.slot(p.slot).is_undef() {
                if let Some(thrown) = self.found_refusal(o, p, name.as_bytes()) {
                    self.release(value);
                    self.release(Value::string(name));
                    return Err(thrown);
                }
                return self.write_std_property(object, name, value);
            }
            // `IS_PROP_LAZY`: a slot that waits is the initializer's
            // first, then the write's on what it answers.
            if self.lazy_slot(o, p.slot) {
                return self.lazy_write(object, name, value, false);
            }
            // `IS_PROP_UNINIT`: a slot nothing has written passes `__set`
            // by; one an `unset` emptied is the setter's.
            if o.slot_uninit(p.slot) {
                return self.write_std_property(object, name, value);
            }
        }
        // `zend_get_property_offset` refusing a slot the scope may not
        // reach, silent only for a class with a `__set` to serve it.
        if let Some(p) = placed.as_ref()
            && !self.reaches(p)
            && !self.serves_accessor(class, b"__set")
        {
            let thrown = self.inaccessible_property(class, p.visibility, name.as_bytes());
            self.release(value);
            self.release(Value::string(name));
            return Err(thrown);
        }
        let bad_name = placed.is_none() && Self::bad_property_name(name.as_bytes());
        if bad_name && !self.serves_accessor(class, b"__set") {
            let thrown = self.bad_property_name_error();
            self.release(value);
            self.release(Value::string(name));
            return Err(thrown);
        }
        let held = placed.is_none()
            && !bad_name
            && o.properties()
                .is_some_and(|table| table.contains(php_heap::Key::Str(name.heap())));
        if held {
            return self.write_std_property(object, name, value);
        }
        // GH-21478: a proxy that forwards, written from inside the
        // instance's own `__set` for this name, writes the instance.
        if let Some(instance) = self.lazy_instance(o)
            && self.guard_word(instance, name.as_bytes()) & IN_SET != 0
        {
            return self.write_property(instance, name, value);
        }
        let Some(setter) = self.magic_accessor(class, b"__set") else {
            if lazy {
                return self.lazy_write(object, name, value, false);
            }
            return self.write_std_property(object, name, value);
        };
        let bytes = name.as_bytes().to_vec();
        let wrong = bad_name || placed.as_ref().is_some_and(|p| !self.reaches(p));
        if self.guard_word(object, &bytes) & IN_SET == 0 {
            let served = self.call_accessor(object, setter, IN_SET, &bytes, &[value]);
            self.release(value);
            self.release(Value::string(name));
            let answered = served?;
            self.release(answered);
            return Ok(());
        }
        // `zend_wrong_offset`: the guard stands, so the placement is
        // made again and the `Error` it raises is the answer.
        if wrong && let Some(thrown) = self.wrong_offset(class, &bytes) {
            self.release(value);
            self.release(Value::string(name));
            return Err(thrown);
        }
        if lazy {
            return self.lazy_write(object, name, value, true);
        }
        // `write_std_property`'s set-visibility arm, which the write a
        // `__set` makes on its own property, under the guard, meets.
        if let Some(placed) = self.declared_slot(class, &bytes)
            && !placed.readonly
            && let Some(set) = placed.set_visibility
            && !self.has_set_access(placed.declaring, set)
        {
            let thrown =
                self.set_inaccessible_property(placed.declaring, set, false, &bytes, "modify");
            self.release(value);
            self.release(Value::string(name));
            return Err(thrown);
        }
        self.write_std_property(object, name, value)
    }

    /// `forward_write_to_lazy_object`
    /// (`Z/Zend/zend_object_handlers.c:1029`): the object initialised,
    /// and the write made on what that answers, under `IN_SET` there
    /// where the `__set` guard stood on the lazy object. `value` and
    /// `name` are given up whatever arm runs.
    fn lazy_write(
        &mut self,
        object: PhpObject,
        name: PhpString,
        value: Value,
        guarded: bool,
    ) -> Result<(), Thrown> {
        let instance = match self.lazy_init(object) {
            Ok(instance) => instance,
            Err(thrown) => {
                self.release(value);
                self.release(Value::string(name));
                return Err(thrown);
            }
        };
        let bytes = name.as_bytes().to_vec();
        if guarded && self.guard_word(instance, &bytes) & IN_SET == 0 {
            self.guard_take(instance, &bytes, IN_SET);
            let written = self.write_property(instance, name, value);
            self.guard_drop(instance, &bytes, IN_SET);
            return written;
        }
        self.write_property(instance, name, value)
    }

    /// `write_std_property`, `zend_std_write_property`'s label and what
    /// a class with no `__set` is: into the declared
    /// slot, what it held released last, or into the dynamic table,
    /// materialised on the first write and separated when a cast shares
    /// it. The name is given up once the table has taken its own owner
    /// of it. A name the table does not hold yet is a creation and goes
    /// through [`Request::dynamic_property_creation`] first, php's two
    /// arms at `write_std_property`'s `else`; the value is given up with
    /// the `Error`, as `FREE_OP_DATA` gives it up.
    ///
    /// A typed slot is `zend_verify_property_type` under
    /// `property_uses_strict_types()` ([`Request::property_strict_types`])
    /// first: the value the declared type accepts, the weak rule's
    /// conversion where the frame the write stands in is not strict,
    /// which a native's own frame never is, and php's `TypeError` with
    /// the value given up where neither holds. The value then lands as
    /// `zend_assign_to_variable_ex` lands it, through the cell where the
    /// slot holds a reference.
    ///
    /// ```php
    /// class Foo { public int $id; public ?float $f = null; }
    /// $db = new PDO('sqlite::memory:');
    /// $db->setAttribute(PDO::ATTR_STRINGIFY_FETCHES, true);
    /// $st = $db->query("SELECT '1' AS id, '2.5' AS f");
    /// var_dump($st->fetchObject(Foo::class));
    /// $st = $db->query("SELECT 'x' AS id");
    /// try { $st->fetchObject(Foo::class); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(Foo)#3 (2) {
    ///   ["id"]=>
    ///   int(1)
    ///   ["f"]=>
    ///   float(2.5)
    /// }
    /// Cannot assign string to property Foo::$id of type int
    /// ```
    ///
    /// The weak rule holds while the native writes, though the file
    /// that called it declares `strict_types`, since php's frame there
    /// is the native's own; a reference the slot holds is written
    /// through.
    ///
    /// ```php
    /// declare(strict_types=1);
    /// class Foo { public int $id = 0; }
    /// $db = new PDO('sqlite::memory:');
    /// $st = $db->query("SELECT '7' AS id");
    /// $st->setFetchMode(PDO::FETCH_INTO, $o = new Foo);
    /// $r = &$o->id;
    /// $st->fetch();
    /// var_dump($o->id, $r);
    /// ```
    /// ```expect
    /// int(7)
    /// int(7)
    /// ```
    ///
    /// A dynamic property that holds a reference a typed property holds
    /// too is written through `zend_assign_to_typed_ref`, an undefined
    /// value as the `null` its warning left.
    ///
    /// ```php
    /// class T { public string $x = ""; public int $n = 1; }
    /// $t = new T; $o = new stdClass;
    /// $o->p =& $t->x;
    /// try { $o->p = $v; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// $o->q =& $t->n;
    /// try { $o->q = "abc"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// $o->q = "5";
    /// var_dump($t->x, $t->n);
    /// ```
    /// ```expect
    ///
    /// Warning: Undefined variable $v in %s on line 5
    /// Cannot assign null to reference held by property T::$x of type string
    /// Cannot assign string to reference held by property T::$n of type int
    /// string(0) ""
    /// int(5)
    /// ```
    fn write_std_property(
        &mut self,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        let o = object.heap();
        if let Some(placed) = self.declared_slot(object.class(), name.as_bytes()) {
            let value = if placed.typed {
                let Some(typed) = self.declared_typed_property(object.class(), name.as_bytes())
                else {
                    self.release(value);
                    self.release(Value::string(name));
                    return self.fatal_of(Err(Fatal::Error(String::from(
                        "InternalError: write_property placed a typed property whose declared type the class does not carry",
                    ))));
                };
                let strict = self.property_strict_types();
                match self.hold_to_property_type_of(object, &typed, name.as_bytes(), value, strict)
                {
                    Ok(value) => value,
                    Err(thrown) => {
                        self.release(Value::string(name));
                        return Err(thrown);
                    }
                }
            } else {
                value
            };
            match self.assign_declared_slot(o, placed.slot, value, self.property_strict_types()) {
                Ok((_, garbage)) => self.release(garbage),
                Err(thrown) => {
                    self.release(Value::string(name));
                    return Err(thrown);
                }
            }
        } else {
            let key = php_heap::Key::Str(name.heap());
            // `zend_array_dup` over a table a clone or a cast shares,
            // before the entry is looked for, so a reference the other
            // holder's copy owns alone is its value here and not a cell
            // both write through.
            if o.properties().is_some_and(|table| table.refcount() > 1) {
                match self.separated_properties(o) {
                    Ok(_) => {}
                    Err(thrown) => {
                        self.release(value);
                        self.release(Value::string(name));
                        return Err(thrown);
                    }
                }
            }
            let held = o.properties().is_some_and(|table| table.contains(key));
            // `zend_assign_to_variable` over the entry found: one that
            // holds a reference, what `$o->p = &$x` bound, is written
            // through, so every name of the cell reads the value, and
            // held to the types of the properties that hold the cell
            // (`zend_assign_to_typed_ref`) under the writing file's rule.
            let bound = o
                .properties()
                .and_then(|table| table.get(key))
                .and_then(|found| Value::of_zval(found).as_php_ref().copied());
            if let Some(cell) = bound {
                self.release(Value::string(name));
                let strict = self.property_strict_types();
                return self.assign_to_reference(cell.heap(), value.undef_as_null(), strict);
            }
            if !held && let Err(thrown) = self.dynamic_property_creation(object, name.as_bytes()) {
                self.release(value);
                self.release(Value::string(name));
                return Err(thrown);
            }
            let table = self.separated_properties(o)?;
            self.array_update(table, key, value)?;
        }
        self.release(Value::string(name));
        Ok(())
    }

    /// `process_nested_object_data`'s write of one property
    /// (`Z/ext/standard/var_unserializer.re:554`): `key` as a serialized
    /// object carries it, the name plain for a public property, `\0*\0`
    /// before a protected one's and `\0Class\0` before a private one's.
    /// A key that names a declared property is written into its slot
    /// directly, visibility, `readonly` and `__set` not consulted, as php
    /// writes `Z_OBJPROP`; one whose mangling names another visibility
    /// than the declaration's is placed by `is_property_visibility_changed`,
    /// by the unmangled name among the class's properties, a parent's
    /// private one included as `ce->properties_info` holds it:
    ///
    /// ```php
    /// class P { private ?string $uri = null; function get() { return $this->uri; } }
    /// #[AllowDynamicProperties] class C extends P {}
    /// var_dump(unserialize('O:1:"C":1:{s:3:"uri";s:1:"X";}')->get());
    /// try { unserialize('O:1:"C":1:{s:3:"uri";i:5;}'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "X"
    /// Cannot assign int to property P::$uri of type ?string
    /// ```
    ///
    /// Any other key is a dynamic
    /// property under the key as written, created through
    /// [`Request::dynamic_property_creation`]'s two arms where the table
    /// does not hold it yet. `value` and `key` are the object's from here;
    /// what the slot held is released after the write.
    pub(crate) fn unserialize_property(
        &mut self,
        object: PhpObject,
        key: PhpString,
        value: Value,
    ) -> Result<bool, Thrown> {
        let class = object.class();
        let bytes = key.as_bytes().to_vec();
        let (mangled_class, name) = match bytes.strip_prefix(b"\0") {
            Some(rest) => match rest.iter().position(|b| *b == 0) {
                Some(end) => (Some(rest[..end].to_vec()), rest[end + 1..].to_vec()),
                None => (None, bytes.clone()),
            },
            None => (None, bytes.clone()),
        };
        let class_name = self.class_name_bytes(class);
        // `is_property_visibility_changed`'s `ZEND_ACC_VIRTUAL` arm: a
        // key the class's table holds a virtual property under has no
        // slot to take the value, and the whole unserialization fails.
        let reaches = match &mangled_class {
            None => true,
            Some(c) => c.as_slice() == b"*" || c.eq_ignore_ascii_case(&class_name),
        };
        if reaches
            && let Some(hooked) = self.hooked(class, &name)
            && hooked.is_virtual
        {
            let declaring = self.class_name(hooked.declaring);
            crate::api::raising::docref(
                self,
                Level::Warning,
                &format!(
                    "Cannot unserialize value for virtual property {declaring}::${}",
                    String::from_utf8_lossy(&name)
                ),
            );
            self.release(value);
            self.release(Value::string(key));
            return Ok(false);
        }
        let slot = self.unserialize_slot(object, &bytes);
        if let Some(at) = slot.and_then(|at| u32::try_from(at).ok()) {
            let z = value.into_zval();
            let o = object.heap();
            let plain = o.slot_bit(at, php_heap::PropBit::PlainRow);
            Heap::object_set_slot(&mut Hooks(self), o, at, z);
            // A plain row stays plain: php writes the table's row itself.
            o.set_slot_bit(at, php_heap::PropBit::PlainRow, plain);
            self.release(Value::string(key));
            return Ok(true);
        }
        let o = object.heap();
        let table_key = php_heap::Key::Str(key.heap());
        let held = o
            .properties()
            .is_some_and(|table| table.contains(table_key));
        if !held && let Err(thrown) = self.dynamic_property_creation(object, &name) {
            self.release(value);
            self.release(Value::string(key));
            return Err(thrown);
        }
        let table = self.separated_properties(o)?;
        self.array_update(table, table_key, value)?;
        self.release(Value::string(key));
        Ok(true)
    }

    /// The declared slot a serialized key places its value in, by its
    /// index among [`Request::placed_properties`]: the property the
    /// mangled name names, or the one of that name whose visibility
    /// changed since (`is_property_visibility_changed`). None where the
    /// table holds copies of the declared rows, whose rows are plain
    /// values, not php's `IS_INDIRECT` to a slot, and the value is
    /// written there with no type to hold to; a slot whose row alone is
    /// plain ([`PhpObject::update_row`]) is written so too.
    fn unserialize_slot(&self, object: PhpObject, key: &[u8]) -> Option<usize> {
        if object.heap().declared_rows() == php_heap::DeclaredRows::Copied {
            return None;
        }
        let class = object.class();
        let (mangled_class, name) = match key.strip_prefix(b"\0") {
            Some(rest) => match rest.iter().position(|b| *b == 0) {
                Some(end) => (Some(&rest[..end]), &rest[end + 1..]),
                None => (None, key),
            },
            None => (None, key),
        };
        let class_name = self.class_name_bytes(class);
        let placed = self.placed_properties(class);
        let exact = placed.iter().rposition(|(declaring, p)| {
            if *p.name.0 != *name {
                return false;
            }
            match (mangled_class, p.visibility) {
                (None, Visibility::Public) => true,
                (Some(c), Visibility::Protected) => c == b"*",
                (Some(c), Visibility::Private) => self.class_name_bytes(*declaring) == c,
                _ => false,
            }
        });
        exact.or_else(|| {
            let reachable =
                mangled_class.is_none_or(|c| c == b"*" || c.eq_ignore_ascii_case(&class_name));
            if !reachable {
                return None;
            }
            // `ce->properties_info` holds a parent's private property
            // too, unless the class declares the name again, which the
            // later row of `placed` is.
            placed.iter().rposition(|(_, p)| *p.name.0 == *name)
        })
    }

    /// `process_nested_object_data`'s hold on a declared typed property
    /// once its value is read (`Z/ext/standard/var_unserializer.re`):
    /// the value held to the type under strict rules, a cell to every
    /// type its other sources hold it to, and the cell made one of this
    /// property's own; a refusal leaves the slot undefined.
    pub(crate) fn unserialize_verify(
        &mut self,
        object: PhpObject,
        key: &[u8],
    ) -> Result<(), Thrown> {
        let Some(at) = self.unserialize_slot(object, key) else {
            return Ok(());
        };
        let placed = self.placed_properties(object.class());
        let Some((declaring, ty, name)) = placed
            .get(at)
            .and_then(|(declaring, p)| Some((*declaring, p.ty.clone()?, p.name.0.to_vec())))
        else {
            return Ok(());
        };
        let Ok(index) = u32::try_from(at) else {
            return Ok(());
        };
        let o = object.heap();
        // A plain row is no `IS_INDIRECT`, so no property info is found
        // for it and nothing is held to a type
        // ([`PhpObject::update_row`]).
        if o.slot_bit(index, php_heap::PropBit::PlainRow) {
            return Ok(());
        }
        let held = Value::of_zval(o.slot(index));
        let checked = match held.as_php_ref() {
            Some(cell) => {
                let cell = cell.heap();
                let own = self
                    .ref_type_sources(cell)
                    .iter()
                    .any(|s| s.declaring == declaring && s.name == name);
                if own {
                    return Ok(());
                }
                crate::ops::verify_prop_assignable_by_ref(self, cell, &ty, declaring, &name, true)
                    .map(|coerced| {
                        if let Some(coerced) = coerced {
                            self.reference_set(cell, coerced);
                        }
                        self.ref_add_type_source(
                            cell,
                            RefSource {
                                declaring,
                                name,
                                ty,
                            },
                        );
                    })
            }
            None => crate::ops::verify_property_type(self, &held, &ty, declaring, &name, true).map(
                |coerced| {
                    if let Some(coerced) = coerced {
                        Heap::object_set_slot(&mut Hooks(self), o, index, coerced.into_zval());
                    }
                },
            ),
        };
        if checked.is_err() {
            Heap::object_set_slot(&mut Hooks(self), o, index, php_heap::Zval::UNDEF);
        }
        checked
    }

    /// `object_and_properties_init` over `zend_symtable_to_proptable`,
    /// the `(object)` cast of an array: each element laid into the new
    /// object's dynamic table under its key as a property name, one
    /// owner of it taken and a reference kept as the reference, with no
    /// handler in the way, so a key a handler would refuse, a mangled
    /// one that opens with a NUL byte, is laid down as it is.
    pub(crate) fn properties_from_array(
        &mut self,
        object: PhpObject,
        array: PhpArray,
    ) -> Result<(), Thrown> {
        for (key, entry) in array.snapshot_refs() {
            let name = key.to_prop_name(self)?;
            self.addref(&entry);
            let o = object.heap();
            let table = self.separated_properties(o)?;
            self.array_update(table, php_heap::Key::Str(name.heap()), entry)?;
            self.release(Value::string(name));
        }
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
    /// deprecation (`GC_ADDREF(obj)`), and where the handler the
    /// deprecation reached gave up every other one, the object is
    /// destroyed and the creation refused with the same `Error`, unless
    /// the handler threw.
    ///
    /// ```php
    /// set_error_handler(function ($code, $msg) { echo "Err: $msg\n"; $GLOBALS['a'] = null; });
    /// $a = new class {};
    /// try { [&$a->y]; } catch (Throwable $ex) { echo "Exception: ", $ex->getMessage(), "\n"; }
    /// var_dump($a);
    /// ```
    /// ```expect
    /// Err: Creation of dynamic property class@anonymous::$y is deprecated
    /// Exception: Cannot create dynamic property class@anonymous::$y
    /// NULL
    /// ```
    pub(crate) fn dynamic_property_creation(
        &mut self,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        use php_unit::decl::DynamicProperties;
        let class = object.class();
        let Some(flags) = self.class_flags(class) else {
            return Ok(());
        };
        let carries = flags.dynamic_properties;
        if carries == DynamicProperties::Allowed {
            return Ok(());
        }
        let shown = String::from_utf8_lossy(name).into_owned();
        let class_name = self.class_name(class);
        if carries == DynamicProperties::Forbidden {
            return Err(crate::api::raising::throw_error(
                self,
                "Error",
                &format!("Cannot create dynamic property {class_name}::${shown}"),
            ));
        }
        let held = self.take_hold(object.as_value());
        crate::api::raising::deprecated(
            self,
            &format!("Creation of dynamic property {class_name}::${shown} is deprecated"),
        );
        if object.as_value().refcount() > 1 {
            self.let_go(held);
            return Ok(());
        }
        self.let_go(held);
        if self.is_pending() {
            return Err(Thrown::of(None));
        }
        Err(crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Cannot create dynamic property {class_name}::${shown}"),
        ))
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
        if o.properties().is_some() {
            self.separated_properties(o)?;
        }
        Ok(())
    }

    /// The start of a `foreach` over an object whose class has no
    /// `get_iterator`, the object arm `FE_RESET_R` and `FE_RESET_RW`
    /// share (`Z/Zend/zend_vm_def.h:6903-6934`): the property table the
    /// walk goes over made the object's own, separated where a cast
    /// shares it and laid down where the object has none, which is php's
    /// `get_properties` materialising it; then, where the object has a
    /// property at all, declared or dynamic, the hash iterator the walk
    /// is registered at (`zend_hash_iterator_add(properties, 0)`), whose
    /// index is the loop variable's `Z_FE_ITER`. `None` is the empty
    /// table php jumps past the loop over with `Z_FE_ITER` at `-1`.
    pub(crate) fn object_walk_start(&mut self, object: PhpObject) -> Result<Option<u32>, Thrown> {
        // `zend_lazy_object_init`: a lazy object is initialised before
        // the walk, which goes over what that answers.
        let object = self.lazy_init(object)?;
        self.separate_object_properties(object)?;
        let o = object.heap();
        let table = self.heap.object_properties(o);
        let table = self.fatal_of(table)?;
        // `Z_OBJPROP_P` of a class whose `get_properties` answers its
        // own rows: the walk stands on the object's table as any does,
        // and goes over what the class answers.
        let handlers = self.object_handlers(o);
        if handlers.properties_overloaded() {
            if handlers.get_properties(self, object)?.is_empty() {
                return Ok(None);
            }
            return Ok(Some(self.heap.iterator_add(table, 0)));
        }
        let slots = match table.declared_rows() {
            php_heap::DeclaredRows::Slots => self.class_slots(object.class()),
            php_heap::DeclaredRows::Copied => 0,
        };
        if slots == 0 && table.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.heap.iterator_add(table, 0)))
    }

    /// `zend_fe_fetch_object_helper` (`Z/Zend/zend_vm_def.h:7058`) over
    /// an object whose class has no `get_iterator`, and the object arm
    /// of `FE_FETCH_RW`: the next property the executing scope may see
    /// from where iterator `idx` stands, with its key and where it sits,
    /// the iterator left one past it. The order is `Z_OBJPROP`'s: the
    /// class's declared instance properties in declaration order, which
    /// php's table carries as its first buckets and this heap walks by
    /// the iterator's lead ([`php_heap::Heap::iterator_lead`]), then the
    /// dynamic table from the iterator's own position, which every write
    /// to the table moves as php's writes move it. A slot nothing has
    /// written is stepped over as php steps over an `IS_UNDEF`
    /// behind an `IS_INDIRECT`, and one the executing scope may not
    /// reach is stepped over as php steps over a mangled key
    /// `zend_check_property_access` refuses. `None` is nothing left,
    /// which is the fetch's jump past the loop. The `Err` is the memory
    /// limit refusing the intern of a declared name; php's key is
    /// interned already and takes no allocation.
    pub(crate) fn object_walk(
        &mut self,
        object: PhpObject,
        idx: u32,
        by_ref: bool,
    ) -> Result<Option<(php_heap::Key, Walked)>, Thrown> {
        let o = object.heap();
        let class = object.class();
        if self.object_handlers(o).properties_overloaded() {
            return self.answered_walk(object, idx);
        }
        // `zend_hooked_object_get_iterator`: a class with a hooked
        // property walks its declared rows in the table's order, each
        // hooked one read through its hook.
        if self.class_has_hooks(class) {
            let mut lead = self.heap.iterator_lead(idx) as usize;
            let found = self.hooked_object_walk(object, &mut lead, by_ref);
            self.heap
                .iterator_set_lead(idx, u32::try_from(lead).unwrap_or(u32::MAX));
            if let Some((key, walked)) = found? {
                return Ok(Some((php_heap::Key::Str(key.heap()), walked)));
            }
        } else {
            // A table that holds copies of the declared rows is walked
            // alone, each of them a row of its own.
            let declared: Vec<(Box<[u8]>, ClassId)> = match o.declared_rows() {
                php_heap::DeclaredRows::Slots => self
                    .placed_properties(class)
                    .iter()
                    .map(|(declaring, p)| (p.name.0.clone(), *declaring))
                    .collect(),
                php_heap::DeclaredRows::Copied => Vec::new(),
            };
            let mut at = self.heap.iterator_lead(idx) as usize;
            while let Some((name, declaring)) = declared.get(at) {
                let slot = u32::try_from(at).unwrap_or(u32::MAX);
                if !o.slot(slot).is_undef()
                    && self.property_row_visible(class, name, Some(*declaring))
                {
                    let interned = self.heap.intern(name);
                    let key = self.fatal_of(interned).map(php_heap::Key::Str)?;
                    self.heap.iterator_set_lead(idx, slot.saturating_add(1));
                    return Ok(Some((key, Walked::Slot(slot))));
                }
                at = at.saturating_add(1);
            }
            self.heap
                .iterator_set_lead(idx, u32::try_from(declared.len()).unwrap_or(u32::MAX));
        }
        let Some(table) = o.properties() else {
            return Ok(None);
        };
        // A dynamic row whose name the scope finds declared, and not
        // public, is stepped over (`zend_check_property_access`).
        let mut pos = self.heap.iterator_pos(idx, table);
        loop {
            let Some((found, key, _)) = table.live_from(pos) else {
                return Ok(None);
            };
            pos = found.saturating_add(1);
            self.heap.iterator_set_pos(idx, pos);
            let visible = match key {
                php_heap::Key::Str(name) => self.property_row_visible(class, name.bytes(), None),
                php_heap::Key::Long(_) => true,
            };
            if visible {
                return Ok(Some((key, Walked::Dynamic(key))));
            }
        }
    }

    /// [`Request::object_walk`] over an object whose class answers its
    /// own rows, `zend_fe_fetch_object_helper` over `Z_OBJPROP_P`, which
    /// is the class's `get_properties` asked again at every fetch: the
    /// next row from the iterator's lead, the rows' index, that the
    /// executing scope may see, a row the class answered for a property
    /// the object holds walked as that slot or that dynamic entry, so a
    /// walk by reference binds it where the std walk does, and any other
    /// row, a `DateInterval`'s field, as the value the class answered.
    ///
    /// ```php
    /// foreach (new DateInterval('P1D') as $k => $v) { echo $k, '=', var_export($v, true), "\n"; }
    /// class D extends DateTime { public $own = 1; protected $hidden = 2; }
    /// $d = new D('2020-01-01');
    /// foreach ($d as $k => &$v) { $v = 5; }
    /// unset($v);
    /// var_dump(get_object_vars($d), $k);
    /// $i = new DateInterval('P2D');
    /// foreach ($i as $k => &$v) { if ($k === 'd') { $v = 9; } }
    /// unset($v);
    /// var_dump($i->d);
    /// ```
    /// ```expect
    /// y=0
    /// m=0
    /// d=1
    /// h=0
    /// i=0
    /// s=0
    /// f=0.0
    /// invert=0
    /// days=false
    /// from_string=false
    /// array(1) {
    ///   ["own"]=>
    ///   int(5)
    /// }
    /// string(3) "own"
    /// int(2)
    /// ```
    fn answered_walk(
        &mut self,
        object: PhpObject,
        idx: u32,
    ) -> Result<Option<(php_heap::Key, Walked)>, Thrown> {
        let class = object.class();
        let o = object.heap();
        let handlers = self.object_handlers(o);
        let rows = handlers.get_properties(self, object)?;
        let placed = self.placed_properties(class);
        let mut cursor = 0;
        let mut at = self.heap.iterator_lead(idx) as usize;
        let mut found = None;
        for (i, (name, value)) in rows.into_iter().enumerate() {
            // The declared slot a row is, by the cursor over the
            // placed properties `object_entries` answers them in.
            let slot = placed
                .iter()
                .skip(cursor)
                .position(|(_, p)| key_names(&name, &p.name.0))
                .map(|found| found + cursor);
            if let Some(slot) = slot {
                cursor = slot + 1;
            }
            if i < at {
                continue;
            }
            at = i + 1;
            let declaring = slot
                .and_then(|slot| placed.get(slot))
                .map(|(declaring, _)| *declaring);
            // An integer key is no name `zend_check_property_access`
            // is asked about.
            if let Key::Str(name) = name
                && !self.property_row_visible(class, name.as_bytes(), declaring)
            {
                continue;
            }
            let key = name.heap();
            let walked = match slot.and_then(|slot| u32::try_from(slot).ok()) {
                Some(slot) if !o.slot(slot).is_undef() => Walked::Slot(slot),
                _ if o.properties().is_some_and(|table| table.contains(key)) => {
                    Walked::Dynamic(key)
                }
                _ => {
                    self.addref(&value);
                    Walked::Answered(value)
                }
            };
            found = Some((key, walked));
            break;
        }
        self.heap
            .iterator_set_lead(idx, u32::try_from(at).unwrap_or(u32::MAX));
        Ok(found)
    }

    /// The key `FE_FETCH` hands a loop over an object's table for the row
    /// [`Request::object_walk`] found: a name as it is, and a mangled
    /// one, which a cast of an array can leave in the dynamic table,
    /// with its class part dropped (`zend_unmangle_property_name_ex`).
    ///
    /// ```php
    /// $obj = (object)["\0A\0b" => 42, "\0*\0c" => 24, "d" => 1];
    /// foreach ($obj as $k => $v) { var_dump($k, $v); }
    /// ```
    /// ```expect
    /// string(1) "b"
    /// int(42)
    /// string(1) "c"
    /// int(24)
    /// string(1) "d"
    /// int(1)
    /// ```
    pub(crate) fn walked_key(&mut self, key: php_heap::Key) -> Result<php_heap::Key, Thrown> {
        let php_heap::Key::Str(name) = key else {
            return Ok(key);
        };
        let crate::natives::Unmangled::Mangled { prop, .. } =
            crate::natives::unmangle_property_name(name.bytes())
        else {
            return Ok(key);
        };
        let interned = self.heap.intern(prop);
        self.fatal_of(interned).map(php_heap::Key::Str)
    }

    /// What [`Request::object_walk`] found at a place: the value, a view
    /// with no owner taken and a reference followed where `deref`.
    pub(crate) fn walked_value(object: PhpObject, walked: Walked) -> Value {
        let o = object.heap();
        let z = match walked {
            Walked::Slot(slot) => o.slot(slot),
            Walked::Dynamic(key) => o
                .properties()
                .and_then(|table| table.get(key))
                .unwrap_or(php_heap::Zval::UNDEF),
            Walked::Hooked(value) | Walked::Answered(value) => return value,
        };
        Value::of_zval(z)
    }

    /// `FE_FETCH_RW`'s bind over an object's property
    /// (`Z/Zend/zend_vm_def.h:7331-7357`, `:7434-7439`): the cell the
    /// place holds, or one made there over the value it held
    /// (`ZVAL_NEW_EMPTY_REF`), the place keeping the owner it had. A
    /// declared `readonly` slot is php's `Error` and no cell; a typed
    /// one puts its type on the cell it makes (`ZEND_REF_ADD_TYPE_SOURCE`),
    /// so a write through the loop's variable is held to it.
    pub(crate) fn walked_reference(
        &mut self,
        object: PhpObject,
        walked: Walked,
    ) -> Result<PhpRef, Thrown> {
        let o = object.heap();
        let held = Request::walked_value(object, walked);
        if let Some(cell) = held.as_php_ref() {
            return Ok(*cell);
        }
        match walked {
            Walked::Slot(slot) => {
                let placed = self.placed_properties(object.class());
                let Some((declaring, decl)) = placed.get(slot as usize).cloned() else {
                    return self.fatal_of(Err(Fatal::Error(format!(
                        "InternalError: a walk over slot {slot}, which the class does not declare"
                    ))));
                };
                if decl.is_readonly {
                    let class = self.class_name(declaring);
                    let shown = String::from_utf8_lossy(&decl.name.0).into_owned();
                    return Err(crate::api::raising::throw_error(
                        self,
                        "Error",
                        &format!("Cannot acquire reference to readonly property {class}::${shown}"),
                    ));
                }
                let cell = self.reference(held)?;
                if let Some(place) = o.slot_ref(slot) {
                    place.write(cell.as_value().to_zval());
                }
                if let Some(ty) = decl.ty.clone() {
                    self.ref_add_type_source(
                        cell.heap(),
                        RefSource {
                            declaring,
                            name: decl.name.0.to_vec(),
                            ty,
                        },
                    );
                }
                Ok(cell)
            }
            Walked::Dynamic(key) => {
                let place = o.properties().and_then(|table| table.value_ref(key));
                let cell = self.reference(held)?;
                if let Some(place) = place {
                    place.write(cell.as_value().to_zval());
                }
                Ok(cell)
            }
            // A hook that answered no reference has no place to bind; the
            // walk refused one whose `get` hook returns none before it
            // ran it ([`Request::hooked_object_walk`]).
            // A row the class answered and the object holds nowhere, a
            // `DateInterval`'s field: php binds it in the table
            // `get_properties` filled, which the next fetch fills again,
            // so the cell stands on its own.
            Walked::Answered(value) => self.reference(value),
            Walked::Hooked(value) => {
                self.release(value);
                self.fatal_of(Err(Fatal::Error(String::from(
                    "InternalError: a hooked property's value bound by reference that its hook did not return by reference",
                ))))
            }
        }
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
    ///
    /// `read` is php's `type`: under [`PropertyRead::Warning`], which is
    /// `BP_VAR_RW` and `BP_VAR_R`, the `Undefined property` warning
    /// stands between the creation's deprecation and the entry
    /// (`Z/Zend/zend_object_handlers.c:1530`), which is the order php
    /// raises the two in and the reverse of the order the pair
    /// [`Request::read_property`] and [`Request::write_property`]
    /// raises them in over the same name. A name the table already
    /// holds raises neither, php returning before both. The entry is
    /// added after the warning, to the table the handler it reached left,
    /// so a value that handler wrote under the name is what the compound
    /// assignment reads.
    ///
    /// ```php
    /// #[AllowDynamicProperties]
    /// class C {
    ///     function error($_, $msg) { echo $msg, "\n"; $this->a = 12345; }
    /// }
    /// $c = new C;
    /// set_error_handler([$c, 'error']);
    /// $c->a %= 10;
    /// var_dump($c->a);
    /// ```
    /// ```expect
    /// Undefined property: C::$a
    /// int(5)
    /// ```
    pub(crate) fn dynamic_property_ref(
        &mut self,
        object: PhpObject,
        name: PhpString,
        read: PropertyRead,
    ) -> Result<Option<php_heap::ZvalRef>, Thrown> {
        let o = object.heap();
        let key = php_heap::Key::Str(name.heap());
        let held = o.properties().is_some_and(|table| table.contains(key));
        if !held && let Err(thrown) = self.dynamic_property_creation(object, name.as_bytes()) {
            self.release(Value::string(name));
            return Err(thrown);
        }
        if !held && read == PropertyRead::Warning {
            let class = self.class_name(object.class());
            let shown = String::from_utf8_lossy(name.as_bytes()).into_owned();
            crate::api::raising::warning(self, &format!("Undefined property: {class}::${shown}"));
        }
        // `zend_hash_add(zobj->properties, name, &EG(uninitialized_zval))`
        // after the warning: the table as the handler the warning reached
        // left it, and a name it wrote kept as it wrote it.
        let table = self.separated_properties(o)?;
        if !table.contains(key) {
            self.array_update(table, key, Value::null())?;
        }
        self.release(Value::string(name));
        Ok(table.value_ref(key))
    }

    /// `EXPECTED(!zobj->ce->__get) || UNEXPECTED((*zend_get_property_guard(zobj,
    /// name)) & IN_GET)` (`Z/Zend/zend_object_handlers.c:1489`) read the
    /// other way round: whether `zend_std_get_property_ptr_ptr` hands
    /// out no place for `name` and leaves the site to `read_property`
    /// and `write_property`, which is a class with a `__get` that the
    /// guard is not already holding.
    pub(crate) fn overloaded_place(&self, object: PhpObject, name: &[u8]) -> bool {
        self.serves_accessor(object.class(), b"__get")
            && (self.guard_word(object, name) & IN_GET == 0
                || self.wrong_name(object.class(), name))
    }

    /// The rows `get_properties` answers for `object`, each beside the
    /// declared slot it is, the class that declared what stands there
    /// and its visibility, or none for a dynamic row: the rows of the
    /// std table are the slots that hold a value, in slot order, and
    /// then the dynamic table, so the slots are walked with a cursor
    /// beside them, as `zend_mangle_property_name` is walked by the
    /// `(array)` cast. Views, no owner taken.
    pub(crate) fn property_rows(&mut self, object: PhpObject) -> Result<Vec<PropertyRow>, Thrown> {
        let table = self.object_handlers(object.heap());
        let rows = table.get_properties(self, object)?;
        // Rows a table holds as copies are plain values to php's
        // `zend_check_property_access`, `is_dynamic` for each.
        let placed = match object.heap().declared_rows() {
            php_heap::DeclaredRows::Slots => self.placed_properties(object.class()),
            php_heap::DeclaredRows::Copied => &[],
        };
        let mut at = 0;
        Ok(rows
            .into_iter()
            .map(|(name, value)| {
                let slot = placed
                    .iter()
                    .skip(at)
                    .position(|(_, p)| key_names(&name, &p.name.0))
                    .map(|i| i + at);
                let declared = slot.and_then(|i| {
                    at = i + 1;
                    placed.get(i).map(|(class, p)| (*class, p.visibility))
                });
                (name, value, declared)
            })
            .collect())
    }

    /// The properties in order, the declared slots that hold a value
    /// then the dynamic table, which is the order
    /// `rebuild_object_properties` lays the table out in. Views, and a
    /// declared name is the interned string php's `prop_info->name` is,
    /// never refcounted, so a caller that takes an owner of it takes
    /// none; the `Err` is the memory limit refusing the intern. A
    /// dynamic row is keyed as the table holds it, so an integer key,
    /// which `object_properties_load` and `zend_hash_copy` write with
    /// `zend_hash_index_update`, stays an integer beside the string that
    /// spells it, and a property access, which names a string, misses it.
    ///
    /// ```php
    /// $g = unserialize('C:3:"GMP":23:{s:1:"2";a:1:{i:46;i:7;}}');
    /// @$g->{46} = 8;
    /// var_dump($g);
    /// foreach ($g as $k => $v) var_dump($k);
    /// var_dump(get_object_vars(clone $g), (array)$g, $g->{46});
    /// ```
    /// ```expect
    /// object(GMP)#1 (3) {
    ///   [46]=>
    ///   int(7)
    ///   ["46"]=>
    ///   int(8)
    ///   ["num"]=>
    ///   string(1) "2"
    /// }
    /// int(46)
    /// string(2) "46"
    /// array(1) {
    ///   [46]=>
    ///   int(8)
    /// }
    /// array(1) {
    ///   [46]=>
    ///   int(8)
    /// }
    /// int(8)
    /// ```
    pub(crate) fn object_entries(&mut self, o: Obj) -> Result<Vec<(Key, Value)>, Thrown> {
        let mut entries = Vec::new();
        // A table that holds copies of the declared rows is read alone,
        // as php reads the plain values its `zend_array_dup` left.
        let declared: Vec<Box<[u8]>> = match o.declared_rows() {
            php_heap::DeclaredRows::Slots => self
                .placed_properties(ClassId(o.class()))
                .iter()
                .map(|(_, p)| p.name.0.clone())
                .collect(),
            php_heap::DeclaredRows::Copied => Vec::new(),
        };
        for (i, name) in declared.iter().enumerate() {
            let z = o.slot(u32::try_from(i).unwrap_or(u32::MAX));
            if z.is_undef() {
                continue;
            }
            let interned = self.heap.intern(name);
            let name = self.fatal_of(interned).map(PhpString::of)?;
            entries.push((Key::Str(name), Value::of_zval(z)));
        }
        if let Some(table) = o.properties() {
            for (k, z) in &table {
                entries.push((Key::of(k), Value::of_zval(z)));
            }
        }
        Ok(entries)
    }

    /// `zend_array_count` over the property table
    /// `zend_std_get_properties` builds for `object` (`Z/Zend/zend_hash.c:519`):
    /// one row per declared slot and one per dynamic property, the
    /// declared slots counted again, the empty ones left out, only where
    /// the table is marked `HASH_FLAG_HAS_EMPTY_IND`, and the mark
    /// cleared where that finds none empty. So a slot `unset` empties
    /// still counts inside the release that empties it, before the mark
    /// is set.
    ///
    /// ```php
    /// class N { public $p = null; public $c = [];
    ///     function __destruct() { var_dump($this); unset($this->c); } }
    /// $a = new N; $a->c[] = new N; $a->c[0]->p = $a;
    /// ```
    /// ```expect
    /// object(N)#1 (2) {
    ///   ["p"]=>
    ///   NULL
    ///   ["c"]=>
    ///   array(1) {
    ///     [0]=>
    ///     object(N)#2 (2) {
    ///       ["p"]=>
    ///       *RECURSION*
    ///       ["c"]=>
    ///       array(0) {
    ///       }
    ///     }
    ///   }
    /// }
    /// object(N)#2 (2) {
    ///   ["p"]=>
    ///   object(N)#1 (2) {
    ///     ["p"]=>
    ///     NULL
    ///   }
    ///   ["c"]=>
    ///   array(0) {
    ///   }
    /// }
    /// ```
    pub(crate) fn property_count(&mut self, object: PhpObject) -> Result<usize, Thrown> {
        let o = object.heap();
        let built = self.heap.object_properties(o);
        let table = self.fatal_of(built)?;
        // A copy holds no `IS_INDIRECT`, so it counts its rows.
        if table.declared_rows() == php_heap::DeclaredRows::Copied {
            return Ok(table.len());
        }
        let declared = self.placed_properties(object.class()).len();
        let total = declared + table.len();
        if !table.has_empty_ind() {
            return Ok(total);
        }
        let held = (0..declared)
            .filter(|at| !o.slot(u32::try_from(*at).unwrap_or(u32::MAX)).is_undef())
            .count();
        let counted = held + table.len();
        if counted == total {
            table.set_has_empty_ind(false);
        }
        Ok(counted)
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
        if object.heap().laziness() != php_heap::Laziness::Eager {
            return self.lazy_clone(object);
        }
        let custom = self.object_custom(object.class());
        let cloned = self.heap.object_clone(object.heap(), custom);
        let Some(new) = self.fatal_of(cloned)? else {
            let class = self.class_name(object.class());
            return self.fatal_of(Err(Fatal::Error(format!(
                "InternalError: Std::clone_obj over an object of class {class}, whose native payload its own table must copy"
            ))));
        };
        let new = PhpObject::of(new);
        self.clone_slot_sources(new);
        if let Err(thrown) = self.serve_clone_method(new) {
            self.release(new.as_value());
            return Err(thrown);
        }
        Ok(new)
    }

    /// `zend_objects_clone_members`' `ZEND_REF_ADD_TYPE_SOURCE`: a slot
    /// of the copy holding a cell a typed property is a source of shares
    /// the cell with the original, and the copy's own property is one
    /// more source of it, so the cell stays held to the type when the
    /// original is gone.
    ///
    /// ```php
    /// class Test { public int $x = 42; }
    /// $test = new Test; $x =& $test->x;
    /// $test2 = clone $test;
    /// unset($test);
    /// try { $x = "foo"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($test2->x);
    /// ```
    /// ```expect
    /// Cannot assign string to reference held by property Test::$x of type int
    /// int(42)
    /// ```
    fn clone_slot_sources(&mut self, copy: PhpObject) {
        let o = copy.heap();
        for slot in 0..o.slots() {
            if let Some(cell) = o.slot(slot).as_reference()
                && cell.sources() != 0
            {
                self.slot_source_added(copy, slot, cell);
            }
        }
    }

    /// The tail of `zend_objects_clone_members`: the class's `__clone`
    /// served on the copy, with every declared slot of it
    /// `IS_PROP_REINITABLE` for the call, so the method may write a
    /// readonly property once more, and none after it, whatever the method
    /// threw. A class with no `__clone` is nothing to serve.
    pub(crate) fn serve_clone_method(&mut self, copy: PhpObject) -> Result<(), Thrown> {
        if self.method_named(copy.class(), b"__clone").is_none() {
            return Ok(());
        }
        Self::mark_reinitable(copy, true);
        let served = crate::api::call::call_method(self, &copy, b"__clone", &[]);
        Self::mark_reinitable(copy, false);
        served.map(|answered| self.release(answered))
    }

    /// `IS_PROP_REINITABLE` set or cleared on every declared slot of
    /// `object`, as `zend_objects_clone_members` and
    /// `zend_objects_clone_obj_with` do around `__clone` and a `clone
    /// with`'s writes.
    fn mark_reinitable(object: PhpObject, on: bool) {
        let o = object.heap();
        for slot in 0..o.slots() {
            o.set_slot_bit(slot, php_heap::PropBit::Reinitable, on);
        }
    }

    /// `zend_objects_clone_members(new, old)` over an object a class's
    /// `clone_obj` made itself: a dynamic table `new` has given up, the
    /// members of `old` copied onto it, what its slots held given up,
    /// and `__clone` served on it.
    pub(crate) fn clone_members_into(
        &mut self,
        new: PhpObject,
        old: PhpObject,
    ) -> Result<(), Thrown> {
        if let Some(table) = self.heap.object_take_properties(new.heap()) {
            self.release(Value::of_zval(Zval::array(table)));
        }
        let displaced = self.heap.object_clone_members_into(new.heap(), old.heap());
        for held in self.fatal_of(displaced)? {
            self.release(Value::of_zval(held));
        }
        self.clone_slot_sources(new);
        self.serve_clone_method(new)
    }

    /// [`PhpObject::clone_with`]: the members copied beside `payload`,
    /// and `__clone` served on the copy.
    pub(crate) fn clone_members_payload<T: Any + Send>(
        &mut self,
        object: PhpObject,
        payload: T,
    ) -> Result<PhpObject, Thrown> {
        let custom = self.object_custom(object.class());
        let cloned = self.heap.object_clone_with(object.heap(), custom, payload);
        let new = PhpObject::of(self.fatal_of(cloned)?);
        self.lay_beside(new)?;
        self.clone_slot_sources(new);
        if let Err(thrown) = self.serve_clone_method(new) {
            self.release(new.as_value());
            return Err(thrown);
        }
        Ok(new)
    }

    /// `zend_objects_clone_obj_with`, the `Std` `clone_obj_with` slot:
    /// the class's `clone_obj`, then each of `properties` written on the
    /// copy as `zend_std_write_property` writes it, a reference with one
    /// owner followed and one with more php's `Error`, under
    /// `EG(fake_scope)` the scope `clone()` was called from, so a
    /// property that scope may not reach is php's `Error` and a parent's
    /// private one a dynamic property; the copy is given up with what a
    /// write threw. Every readonly property is writable once more for
    /// the writes, and each is locked again by the write that lands on
    /// it, so a hook that initialised one leaves it closed to the next.
    ///
    /// ```php
    /// class P { public $a = 'a'; protected $b = 'b'; private $c = 'c';
    ///     public function __construct(public readonly int $r = 1) {}
    ///     function with() { return clone($this, ['b' => 'B', 'c' => 'C', 'r' => 2]); }
    ///     function again() { return clone($this, ['r' => 3]); } }
    /// class C extends P { function mine() { return clone($this, ['c' => 'dynamic']); } }
    /// var_dump((new P)->with());
    /// try { clone(new P, ['b' => 'x']); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { clone(new P, ['r' => 3]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump((new P)->again()->r);
    /// #[AllowDynamicProperties] class D extends C {}
    /// var_dump((new D)->mine());
    /// ```
    /// ```expect
    /// object(P)#2 (4) {
    ///   ["a"]=>
    ///   string(1) "a"
    ///   ["b":protected]=>
    ///   string(1) "B"
    ///   ["c":"P":private]=>
    ///   string(1) "C"
    ///   ["r"]=>
    ///   int(2)
    /// }
    /// Cannot access protected property P::$b
    /// Cannot modify protected(set) readonly property P::$r from global scope
    /// int(3)
    /// object(D)#3 (5) {
    ///   ["a"]=>
    ///   string(1) "a"
    ///   ["b":protected]=>
    ///   string(1) "b"
    ///   ["c":"P":private]=>
    ///   string(1) "c"
    ///   ["r"]=>
    ///   int(1)
    ///   ["c"]=>
    ///   string(7) "dynamic"
    /// }
    /// ```
    pub(crate) fn clone_members_with(
        &mut self,
        object: PhpObject,
        scope: Option<ClassId>,
        properties: PhpArray,
    ) -> Result<PhpObject, Thrown> {
        let table = self.object_handlers(object.heap());
        let Some(clone_obj) = table.clone_obj() else {
            return Err(self.uncloneable(object));
        };
        let new = clone_obj(self, object)?;
        // "Unlock readonly properties once more" for the writes, and lock
        // them again after, whatever a write threw; `EG(fake_scope) =
        // scope` for the writes and put back after them.
        Self::mark_reinitable(new, true);
        let saved = std::mem::replace(&mut self.fake_scope, scope);
        let written = self.clone_with_writes(new, table, properties);
        self.fake_scope = saved;
        Self::mark_reinitable(new, false);
        if let Err(thrown) = written {
            self.release(new.as_value());
            return Err(thrown);
        }
        Ok(new)
    }

    /// The writes of [`Request::clone_members_with`] over the copy, each
    /// through the class's `write_property`.
    fn clone_with_writes(
        &mut self,
        new: PhpObject,
        table: &'static dyn crate::api::abi::ObjectHandlers,
        properties: PhpArray,
    ) -> Result<(), Thrown> {
        for (key, value) in properties.snapshot_refs() {
            let value = match value.as_php_ref() {
                Some(r) if r.as_value().refcount() > 1 => {
                    return Err(crate::api::raising::throw_error(
                        self,
                        "Error",
                        "Cannot assign by reference when cloning with updated properties",
                    ));
                }
                Some(_) => value.deref(),
                None => value,
            };
            let name = key.to_prop_name(self)?;
            self.addref(&value);
            self.write_cloned_property(new, table, name, value)?;
        }
        Ok(())
    }

    /// `zend_std_write_property` over one property of a `clone with`:
    /// where the name is the standard place, it is placed first
    /// ([`Request::declared_slot_for_write`]), which raises what the
    /// scope may not reach, a readonly property already written and a
    /// set visibility the scope lacks, and a declared slot the write
    /// lands on is locked again (`Z_PROP_FLAG_P(variable_ptr) &=
    /// ~IS_PROP_REINITABLE`); the write itself is the class's
    /// `write_property`, which serves a hook, a `__set` and a dynamic
    /// property.
    fn write_cloned_property(
        &mut self,
        object: PhpObject,
        table: &'static dyn crate::api::abi::ObjectHandlers,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        let bytes = name.as_bytes().to_vec();
        let place = match table.get_property_ptr_ptr(self, object, &bytes) {
            Ok(place) => place,
            Err(thrown) => {
                self.release(value);
                self.release(Value::string(name));
                return Err(thrown);
            }
        };
        if place == crate::api::abi::PropertyPlace::Handlers {
            return table.write_property(self, object, name, value);
        }
        let silent = self.serves_accessor(object.class(), b"__set");
        let slot = match self.declared_slot_for_write(object, &bytes, "modify", silent) {
            Ok(placed) => placed.map(|(slot, _)| slot),
            Err(thrown) => {
                self.release(value);
                self.release(Value::string(name));
                return Err(thrown);
            }
        };
        table.write_property(self, object, name, value)?;
        if let Some(slot) = slot {
            object
                .heap()
                .set_slot_bit(slot, php_heap::PropBit::Reinitable, false);
        }
        Ok(())
    }

    /// php's `Error` for a class whose `clone_obj` slot is null.
    fn uncloneable(&mut self, object: PhpObject) -> Thrown {
        let class = self.class_name(object.class());
        crate::api::raising::throw_error(
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
                    return Err(crate::api::raising::throw_error(
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
        let custom = self.object_custom(object.class());
        let cloned = self.heap.object_clone_with(object.heap(), custom, payload);
        let new = self.fatal_of(cloned).map(PhpObject::of)?;
        self.lay_beside(new)?;
        Ok(new)
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

    /// [`Request::native_mut`], or the `InternalError` of a method served
    /// on an object without the payload. The first read answers whether
    /// it is there, so the fatal is raised with the request free; the
    /// second cannot miss, since nothing runs between the two.
    pub(crate) fn native_mut_or_fatal<T: std::any::Any + Send>(
        &mut self,
        o: Obj,
    ) -> Result<&mut T, Thrown> {
        if self.heap.native::<T>(o).is_none() {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a method served on an object without its payload",
            ));
        }
        self.heap.native_mut(o).ok_or(Thrown::of(None))
    }

    /// `zend_objects_destroy_object` (`Z/Zend/zend_objects.c:114`): the
    /// class's `__destruct` served on the object, which the store holds
    /// at one owner meanwhile (`zend_objects_store_del` holds the count
    /// there, where php's `GC_ADDREF` and `OBJ_RELEASE` around the call
    /// hold it). A class with none serves nothing, and so does a lazy
    /// object, which is the first test php makes after finding the
    /// destructor (`zend_object_is_lazy`). The visibility check and the
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
        let class = object.class();
        let Some(destructor) = self.method_named(class, b"__destruct") else {
            return;
        };
        // `zend_object_is_lazy`: a lazy object's destructor is not
        // served, nor a forwarding proxy's, whose instance serves its own.
        if object.heap().laziness() != php_heap::Laziness::Eager {
            return;
        }
        if !self.destructor_reachable(object, destructor) {
            return;
        }
        let saved = self.save_pending(object);
        if matches!(saved, Saved::TheObject) {
            return;
        }
        if let Err(thrown) = self.serve_method(destructor, Some(object), class, &[], &[]) {
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
        self.shutdown_walk_ended = true;
        self.heap.mark_destructed();
        if let Err(fatal) = self.uncaught() {
            self.end_in(&fatal);
        }
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
        let thrown = crate::api::raising::throw_error(
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
    /// A frame being popped is not one: `zend_leave_helper` has made its
    /// caller current before it frees the CVs (`Z/Zend/zend_vm_def.h:2970`),
    /// so a destructor a top frame's last release serves at shutdown
    /// finds nothing executing, and what it raises is the uncaught fatal
    /// `zend_call_function`'s tail renders there, the throwable the
    /// frame was left with never chained under it.
    ///
    /// ```php
    /// class C {
    ///     public function __destruct() {
    ///         static $n = 0;
    ///         if (++$n === 1) { $c = new C; }
    ///         throw new Exception("from destructor $n");
    ///     }
    /// }
    /// $c = new C;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Exception: from destructor 2 in %s:6
    /// Stack trace:
    /// #0 [internal function]: C->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 6
    /// ```
    pub fn executing(&self) -> bool {
        self.fibers.active().is_some()
            || self.vm.frames().iter().any(|frame| !frame.leaving)
            || self.vm.serving_call().is_some()
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
            let symbols = self.globals().len();
            let mut at = symbols;
            while at > 0 && !self.shutdown_walk_ended {
                at -= 1;
                let alone = self.globals().get(at).is_some_and(|(_, symbol)| {
                    let value = match symbol {
                        Symbol::Value(value) => *value,
                        Symbol::Held(cell) => cell.get(),
                        Symbol::Indirect(..) => return false,
                    };
                    value.as_object().is_some() && value.refcount() == 1
                });
                if alone {
                    let (_, symbol) = self.globals().remove(at);
                    self.release_symbol(symbol);
                }
            }
            if symbols == self.globals().len() || self.shutdown_walk_ended {
                break;
            }
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

    /// Whether no extension registered a handler table for `class` or a
    /// class it extends, so its dimensions are `zend_std_read_dimension`'s,
    /// whose failure answers `NULL`, where a table of an extension's
    /// answers what its own C handler does (`spl_array_read_dimension_ex`
    /// answers `&EG(uninitialized_zval)`).
    pub(crate) fn std_dimensions(&self, class: ClassId) -> bool {
        let mut at = Some(class);
        for _ in 0..64 {
            let Some(class) = at else {
                return true;
            };
            if let Some(Some(_)) = usize::try_from(class.0)
                .ok()
                .and_then(|i| self.shared.handlers.get(i))
            {
                return false;
            }
            at = self.class_parent(class);
        }
        true
    }

    /// The handler table an object of `class` is reached through
    /// (design §5.4): the class's own where it declared one, else the
    /// nearest ancestor's, as php inherits `create_object` and the table
    /// it installs, and `Std`'s at a root and for every user class. The
    /// walk is cut at a depth no declaration can make.
    pub(crate) fn handlers_of(&self, class: ClassId) -> &'static dyn ObjectHandlers {
        self.own_handlers(class).unwrap_or(&Std)
    }

    /// The handler table of `class` where it is not
    /// `std_object_handlers`: its own or the nearest parent's, or an
    /// enum's; `None` where its objects are served by [`Std`].
    pub(crate) fn own_handlers(&self, class: ClassId) -> Option<&'static dyn ObjectHandlers> {
        let mut at = Some(class);
        for _ in 0..64 {
            let Some(class) = at else {
                break;
            };
            if let Some(Some(table)) = usize::try_from(class.0)
                .ok()
                .and_then(|i| self.shared.handlers.get(i))
            {
                return Some(*table);
            }
            at = self.class_parent(class);
        }
        self.class_decl(class)
            .is_some_and(|decl| decl.kind == ClassKind::Enum)
            .then_some(&crate::enums::ENUM as &'static dyn ObjectHandlers)
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

    /// Whether objects of `class` are made by `zend_default_exception_new`,
    /// which every class under `Throwable` reaches: the throwables' table
    /// its own or an ancestor's, under any table an extension gives a
    /// subclass, whose `create_object` wraps it as php's
    /// `ce->create_object` wraps `zend_ce_exception->create_object`.
    pub(crate) fn records_throw_site(&self, class: ClassId) -> bool {
        let mut at = Some(class);
        for _ in 0..64 {
            let Some(class) = at else {
                return false;
            };
            if let Some(Some(table)) = usize::try_from(class.0)
                .ok()
                .and_then(|i| self.shared.handlers.get(i))
                && crate::classes::records_throw_site(*table)
            {
                return true;
            }
            at = self.class_parent(class);
        }
        false
    }

    /// [`Request::handlers_of`] for the object's class, php's
    /// `zobj->handlers`.
    pub(crate) fn object_handlers(&self, o: Obj) -> &'static dyn ObjectHandlers {
        self.handlers_of(ClassId(o.class()))
    }

    /// `zend_std_has_property`: the declared slot where the scope may
    /// reach it and it holds a value, else the dynamic table's entry;
    /// then what `test` asks of the value: that it is there, that it is
    /// not `null`, or that it is truthy. A slot the scope may not reach
    /// or nothing wrote is the class's `__isset`, whose answer's truth
    /// is the answer; php places the name silently here whatever the
    /// class declares (`zend_get_property_offset(ce, name, 1, ...)`),
    /// so the property the scope may not reach reaches the accessor and
    /// never the `Error`. Under [`PropertyTest::Exists`],
    /// `property_exists()`'s `ZEND_PROPERTY_EXISTS`, no accessor is
    /// called at all.
    ///
    /// Under [`PropertyTest::NotEmpty`], `empty($o->p)`, a `__isset`
    /// that said yes is followed by `__get`, whose value's truth is the
    /// answer, and a class with no `__get` answers `false`; both are
    /// served under the guard the `__isset` still holds, so an
    /// accessor that asks about its own name answers for itself.
    ///
    /// ```php
    /// class M {
    ///     public $kept = 1;
    ///     public function __isset($k) { echo "isset $k\n"; return $k === "y"; }
    ///     public function __get($k) { echo "get $k\n"; return 0; }
    /// }
    /// $m = new M;
    /// var_dump(isset($m->kept), isset($m->y), isset($m->nope));
    /// var_dump(empty($m->y), empty($m->nope));
    /// ```
    /// ```expect
    /// isset y
    /// isset nope
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// isset y
    /// get y
    /// isset nope
    /// bool(true)
    /// bool(true)
    /// ```
    pub(crate) fn has_property(
        &mut self,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        let class = object.class();
        let o = object.heap();
        if let Some(hooked) = self.hooked(class, name)
            && self.hooked_accessible(&hooked)
        {
            return self.has_hooked(object, name, test, hooked);
        }
        // php's `value`, the slot the placement found, which keeps a
        // lazy object from initialising unless the slot itself waits.
        let mut placed_slot = None;
        let value = match self.declared_slot(class, name) {
            Some(placed) => {
                if self.reaches(&placed) {
                    placed_slot = Some(placed.slot);
                    let z = o.slot(placed.slot);
                    (!z.is_undef()).then(|| Value::of_zval(z))
                } else {
                    None
                }
            }
            // A name the scope finds no declaration of is the dynamic
            // table's alone, a parent's private slot of the same name
            // being none of its business.
            None if Self::bad_property_name(name) => None,
            None => Self::dynamic_prop(o, name),
        };
        if let Some(value) = value {
            let value = value.deref();
            return Ok(match test {
                PropertyTest::Exists => true,
                PropertyTest::Isset => value.tag() != TypeTag::Null,
                PropertyTest::NotEmpty => crate::api::coerce::truthy(self, &value),
            });
        }
        // `IS_PROP_UNINIT`: "Skip __isset() for uninitialized typed
        // properties", which answer no whatever the test; a slot that
        // waits on a lazy initializer is `IS_PROP_LAZY` first, which
        // initialises the object below.
        if placed_slot.is_some_and(|slot| o.slot_uninit(slot) && !self.lazy_slot(o, slot)) {
            return Ok(false);
        }
        // `lazy_init:`, which a slot that waits reaches past `__isset`
        // and a name the placement found no slot for reaches where no
        // accessor answers it.
        let initialises = |req: &Request| match placed_slot {
            Some(slot) => req.lazy_slot(o, slot),
            None => o.laziness() != php_heap::Laziness::Eager,
        };
        if placed_slot.is_some_and(|slot| self.lazy_slot(o, slot)) {
            return self.lazy_has(object, name, test);
        }
        // GH-21478: a proxy that forwards, asked from inside the
        // instance's own `__isset` for this name, asks the instance.
        if let Some(instance) = self.lazy_instance(o)
            && self.guard_word(instance, name) & IN_ISSET != 0
        {
            return self.has_property(instance, name, test);
        }
        let issetter = self.magic_accessor(class, b"__isset");
        if issetter.is_some() && test == PropertyTest::Exists {
            return Ok(false);
        }
        let Some(issetter) = issetter else {
            if initialises(self) {
                return self.lazy_has(object, name, test);
            }
            return Ok(false);
        };
        if self.guard_word(object, name) & IN_ISSET != 0 {
            if initialises(self) {
                return self.lazy_has(object, name, test);
            }
            return Ok(false);
        }
        // php holds `IN_ISSET` across both calls, so the `__get` the
        // `empty` arm asks for runs with the `__isset`'s guard still
        // standing, and the object across both (`GC_ADDREF(zobj)`,
        // `OBJ_RELEASE` after the guard is down), so an `__isset` that
        // gives up every other owner does not free the object the
        // `__get` is served on.
        self.guard_take(object, name, IN_ISSET);
        let held = self.take_hold(object.as_value());
        let answered = self.isset_then_truth(object, name, test, issetter);
        self.guard_drop(object, name, IN_ISSET);
        self.release_hold(held);
        answered
    }

    /// `zend_std_has_property`'s `/* have issetter - try with it! */`
    /// arm, under the guard and the hold its caller takes: the truth of
    /// what the `__isset` answered, and under `empty` the truth of what
    /// the `__get` answers after a yes; a class with no `__get`, or one
    /// whose guard is up, is `result = false`, php's own `else`.
    ///
    /// ```php
    /// class Test {
    ///     public function __isset($x) { $GLOBALS["obj"] = 24; return true; }
    ///     public function __get($x) { var_dump($this); return 42; }
    /// }
    /// $obj = new Test;
    /// $name = "foo";
    /// var_dump(empty($obj->$name));
    /// var_dump($obj);
    /// ```
    /// ```expect
    /// object(Test)#1 (0) {
    /// }
    /// bool(false)
    /// int(24)
    /// ```
    fn isset_then_truth(
        &mut self,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
        issetter: Function,
    ) -> Result<bool, Thrown> {
        let answered = self.serve_accessor(object, issetter, name, &[])?;
        let truthy = crate::api::coerce::truthy(self, &answered);
        self.release(answered);
        if test != PropertyTest::NotEmpty || !truthy {
            return Ok(truthy);
        }
        let Some(getter) = self
            .magic_accessor(object.class(), b"__get")
            .filter(|_| self.guard_word(object, name) & IN_GET == 0)
        else {
            return Ok(false);
        };
        let answered = self.call_accessor(object, getter, IN_GET, name, &[])?;
        let truthy = crate::api::coerce::truthy(self, &answered);
        self.release(answered);
        Ok(truthy)
    }

    /// `zend_std_has_property`'s `lazy_init:` arm: the object
    /// initialised and asked again on what that answers, under
    /// `IN_ISSET` there where its class has a `__isset`, so the accessor
    /// is not served twice for one question.
    ///
    /// ```php
    /// class C { public $a = 1; }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function ($o) { echo "init o\n"; });
    /// var_dump(isset($o->nope));
    /// $p = $r->newLazyProxy(function () { echo "factory\n"; return new C; });
    /// unset($p->a);
    /// var_dump($p);
    /// ```
    /// ```expect
    /// init o
    /// bool(false)
    /// factory
    /// lazy proxy object(C)#4 (1) {
    ///   ["instance"]=>
    ///   object(C)#5 (0) {
    ///   }
    /// }
    /// ```
    fn lazy_has(
        &mut self,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        let instance = self.lazy_init(object)?;
        if self.serves_accessor(instance.class(), b"__isset")
            && self.guard_word(instance, name) & IN_ISSET == 0
        {
            self.guard_take(instance, name, IN_ISSET);
            let held = self.has_property(instance, name, test);
            self.guard_drop(instance, name, IN_ISSET);
            return held;
        }
        self.has_property(instance, name, test)
    }

    /// `zend_std_unset_property`: a declared slot the scope may reach is
    /// left uninitialised, what it held released; one it may not reach
    /// is php's `Error`, and so is one whose set visibility the scope
    /// may not write, whose word here is `unset`
    /// (`Z/Zend/zend_object_handlers.c:1583`); a dynamic name is
    /// removed from the table, separated first where a cast shares it.
    ///
    /// A class with a `__unset` is served where the placement found
    /// nothing to remove: a name no class declared that the table does
    /// not hold, a declared untyped slot an `unset` already emptied,
    /// and, since the placement is made silent by the accessor
    /// (`zend_get_property_offset(ce, name, ce->__unset != NULL, ...)`),
    /// a declared slot the scope may not reach. It is served under
    /// `IN_UNSET`, so an `__unset` unsetting the name it was called for
    /// removes nothing rather than reaching itself, and the `Error` for
    /// the property the scope may not reach is raised on that second
    /// pass (`zend_wrong_offset`).
    ///
    /// ```php
    /// class M {
    ///     public $kept = 1;
    ///     public function __unset($k) { echo "unset $k\n"; }
    /// }
    /// $m = new M;
    /// $m->dyn = 2;
    /// unset($m->kept);
    /// unset($m->dyn);
    /// unset($m->nope);
    /// var_dump($m);
    /// class U { public function __unset($k) { echo "unset $k\n"; unset($this->w); } }
    /// $u = new U;
    /// unset($u->w);
    /// ```
    /// ```expect
    ///
    /// Deprecated: Creation of dynamic property M::$dyn is deprecated in %s on line 7
    /// unset nope
    /// object(M)#1 (0) {
    /// }
    /// unset w
    /// ```
    ///
    /// `IS_PROP_UNINIT` tells a typed slot nothing has written, which
    /// passes the magic methods by, from one an `unset` emptied, which
    /// reaches them; the set visibility is checked where php checks it,
    /// inside the accessor, and names the class that declared the
    /// property:
    ///
    /// ```php
    /// class C { public private(set) int $a = 1; public int $u;
    ///     function __construct() { unset($this->a); } }
    /// class D extends C {
    ///     function __unset($n) { echo "__unset $n\n"; unset($this->a); }
    ///     function __get($n) { echo "__get $n\n"; return 7; }
    /// }
    /// $d = new D;
    /// try { unset($d->a); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($d->a);
    /// try { var_dump($d->u); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// unset($d->u);
    /// var_dump($d->u);
    /// ```
    /// ```expect
    /// __unset a
    /// Cannot unset private(set) property C::$a from scope D
    /// __get a
    /// int(7)
    /// Typed property C::$u must not be accessed before initialization
    /// __get u
    /// int(7)
    /// ```
    /// `zend_std_unset_property`'s declared arm over a slot that holds
    /// a value: the value given up with the slot left `UNDEF`, no longer
    /// `IS_PROP_UNINIT` whatever it was made with, and then
    /// `HT_FLAGS(zobj->properties) |= HASH_FLAG_HAS_EMPTY_IND`, so a
    /// destructor the release served counted the slot still
    /// ([`Request::property_count`]).
    fn empty_slot(&mut self, object: PhpObject, slot: u32) {
        let o = object.heap();
        if let Some(cell) = o.slot(slot).as_reference() {
            self.slot_source_gone(object, slot, cell);
        }
        Heap::object_set_slot(&mut Hooks(self), o, slot, Zval::UNDEF);
        o.set_slot_bit(slot, php_heap::PropBit::Uninit, false);
        if let Some(table) = o.properties() {
            table.set_has_empty_ind(true);
        }
    }

    pub(crate) fn unset_property(&mut self, object: PhpObject, name: &[u8]) -> Result<(), Thrown> {
        let class = object.class();
        let o = object.heap();
        if let Some(hooked) = self.hooked(class, name)
            && self.hooked_accessible(&hooked)
        {
            return Err(self.unset_hooked(object, name));
        }
        let unsetter = self.magic_accessor(class, b"__unset");
        let mut wrong = false;
        if let Some(placed) = self.declared_slot(class, name) {
            if self.reaches(&placed) {
                let empty = o.slot(placed.slot).is_undef();
                // `error = Z_TYPE_P(slot) != IS_UNDEF || (Z_PROP_FLAG_P(slot)
                // & IS_PROP_UNINIT) || !zobj->ce->__unset`: a slot that
                // holds a value, and a typed one nothing has written,
                // are held to the set visibility here; an untyped slot
                // an `unset` emptied is the `__unset`'s.
                let waits = self.lazy_slot(o, placed.slot);
                let uninit = o.slot_uninit(placed.slot);
                let in_unset = unsetter.is_some() && self.guard_word(object, name) & IN_UNSET != 0;
                if !empty || uninit || unsetter.is_none() || in_unset || waits {
                    // `zend_readonly_property_unset_error`, ahead of the
                    // set visibility: an initialised readonly property is
                    // unset from no scope, the class's own included, save
                    // inside `__clone`, where the slot is
                    // `IS_PROP_REINITABLE`.
                    if placed.readonly
                        && !empty
                        && !o.slot_bit(placed.slot, php_heap::PropBit::Reinitable)
                    {
                        let declaring = self.class_name(placed.declaring);
                        let shown = String::from_utf8_lossy(name).into_owned();
                        return Err(crate::api::raising::throw_error(
                            self,
                            "Error",
                            &format!("Cannot unset readonly property {declaring}::${shown}"),
                        ));
                    }
                    if let Some(set) = placed.set_visibility
                        && !self.has_set_access(placed.declaring, set)
                    {
                        return Err(self.set_inaccessible_property(
                            placed.declaring,
                            set,
                            placed.readonly,
                            name,
                            "unset",
                        ));
                    }
                    if !empty {
                        self.empty_slot(object, placed.slot);
                    } else if uninit && !waits {
                        // `Z_PROP_FLAG_P(slot) = 0`: the bit reset and
                        // `__unset` passed by.
                        o.set_slot_bit(placed.slot, php_heap::PropBit::Uninit, false);
                    } else if waits {
                        // `IS_PROP_LAZY`: the object initialised and the
                        // property unset on what that answers.
                        let instance = self.lazy_init(object)?;
                        return self.unset_property(instance, name);
                    }
                    return Ok(());
                }
            } else {
                if unsetter.is_none() {
                    return Err(self.inaccessible_property(class, placed.visibility, name));
                }
                wrong = true;
            }
        } else if let Some(miss) = self.unplaced(class, name, unsetter.is_some()) {
            if unsetter.is_none() {
                return Err(self.unplaced_error(class, name, miss));
            }
            wrong = true;
        } else if let Some(table) = o.properties() {
            let key = self.interned(name)?;
            let key = php_heap::Key::Str(key.heap());
            if table.contains(key) {
                let table = self.separated_properties(o)?;
                self.array_del(table, key);
                return Ok(());
            }
        }
        // GH-21478: a proxy that forwards, unset from inside the
        // instance's own `__unset` for this name, unsets the instance's.
        if let Some(instance) = self.lazy_instance(o)
            && self.guard_word(instance, name) & IN_UNSET != 0
        {
            return self.unset_property(instance, name);
        }
        if let Some(unsetter) = unsetter {
            if self.guard_word(object, name) & IN_UNSET == 0 {
                let answered = self.call_accessor(object, unsetter, IN_UNSET, name, &[])?;
                self.release(answered);
                return Ok(());
            }
            if wrong && let Some(thrown) = self.wrong_offset(class, name) {
                return Err(thrown);
            }
        }
        // A lazy object with nothing that answered the name is
        // initialised and the name unset on what that answers, under
        // `IN_UNSET` there where the `__unset` guard stood here.
        if o.laziness() != php_heap::Laziness::Eager {
            let instance = self.lazy_init(object)?;
            if unsetter.is_some() && self.guard_word(instance, name) & IN_UNSET == 0 {
                self.guard_take(instance, name, IN_UNSET);
                let unset = self.unset_property(instance, name);
                self.guard_drop(instance, name, IN_UNSET);
                return unset;
            }
            return self.unset_property(instance, name);
        }
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
        Err(crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Cannot use object of type {class} as array"),
        ))
    }

    /// What the four `zend_std_*_dimension` handlers do around the
    /// `ArrayAccess` methods they call: the offset copied, dereferenced
    /// (`ZVAL_COPY_DEREF(&tmp_offset, offset)`), and the object held
    /// (`GC_ADDREF(object)`) for the span of `calls`, both given back
    /// after it (`OBJ_RELEASE`, `zval_ptr_dtor(&tmp_offset)`), so a
    /// method that gives up every other owner of either does not free
    /// what the next call is served on or handed.
    fn with_array_access<T>(
        &mut self,
        object: PhpObject,
        offset: Value,
        calls: impl FnOnce(&mut Request, Value) -> Result<T, Thrown>,
    ) -> Result<T, Thrown> {
        let offset = self.take_hold(offset.deref());
        let held = self.take_hold(object.as_value());
        let called = calls(self, offset.value());
        self.release_hold(held);
        self.release_hold(offset);
        called
    }

    /// `zend_std_read_dimension`: `offsetGet` on an `ArrayAccess`, the
    /// offset `null` for `$o[]`; under `BP_VAR_IS` `offsetExists` first,
    /// `null` where it says no, and `offsetGet` after it where it says
    /// yes, so a chain under an `isset` calls the two in that order and
    /// calls neither on a class that is no `ArrayAccess`. The object and
    /// the offset are held across the two ([`Request::with_array_access`]).
    ///
    /// ```php
    /// class Test implements ArrayAccess {
    ///     public function offsetExists($x): bool { $GLOBALS["obj"] = 24; return true; }
    ///     public function offsetGet($x): mixed { var_dump($x); return 42; }
    ///     public function offsetSet($x, $y): void { }
    ///     public function offsetUnset($x): void { }
    /// }
    /// $obj = new Test;
    /// $name = "foo";
    /// var_dump($obj[$name] ?? 12);
    /// var_dump($obj);
    /// ```
    /// ```expect
    /// string(3) "foo"
    /// int(42)
    /// int(24)
    /// ```
    pub(crate) fn read_dimension(
        &mut self,
        object: PhpObject,
        offset: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Value, Thrown> {
        self.array_access(object)?;
        let offset = offset.copied().unwrap_or_else(Value::null);
        self.with_array_access(object, offset, |req, offset| {
            if read == DimensionRead::Silent {
                let exists = crate::api::call::call_method(
                    req,
                    &object,
                    b"offsetExists",
                    std::slice::from_ref(&offset),
                )?;
                let answer = crate::api::coerce::truthy(req, &exists);
                req.release(exists);
                if !answer {
                    return Ok(Value::null());
                }
            }
            crate::api::call::call_method(req, &object, b"offsetGet", &[offset])
        })
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
        let served = self.with_array_access(object, offset, |req, offset| {
            crate::api::call::call_method(req, &object, b"offsetSet", &[offset, value])
        });
        self.release(value);
        self.release(served?);
        Ok(())
    }

    /// `zend_std_has_dimension`: `offsetExists` on an `ArrayAccess`,
    /// and under `not_empty` the truth of `offsetGet` after it where
    /// nothing was thrown, the object and the offset held across the two.
    ///
    /// ```php
    /// class Test implements ArrayAccess {
    ///     public function offsetExists($x): bool { $GLOBALS["obj"] = 24; return true; }
    ///     public function offsetGet($x): mixed { var_dump($x); return 42; }
    ///     public function offsetSet($x, $y): void { }
    ///     public function offsetUnset($x): void { }
    /// }
    /// $obj = new Test;
    /// $name = "foo";
    /// var_dump(empty($obj[$name]));
    /// var_dump($obj);
    /// ```
    /// ```expect
    /// string(3) "foo"
    /// bool(false)
    /// int(24)
    /// ```
    pub(crate) fn has_dimension(
        &mut self,
        object: PhpObject,
        offset: &Value,
        not_empty: bool,
    ) -> Result<bool, Thrown> {
        self.array_access(object)?;
        self.with_array_access(object, *offset, |req, offset| {
            let exists = crate::api::call::call_method(
                req,
                &object,
                b"offsetExists",
                std::slice::from_ref(&offset),
            )?;
            let answer = crate::api::coerce::truthy(req, &exists);
            req.release(exists);
            if !answer || !not_empty {
                return Ok(answer);
            }
            let value = crate::api::call::call_method(
                req,
                &object,
                b"offsetGet",
                std::slice::from_ref(&offset),
            )?;
            let answer = crate::api::coerce::truthy(req, &value);
            req.release(value);
            Ok(answer)
        })
    }

    /// `zend_std_unset_dimension`: `offsetUnset` on an `ArrayAccess`,
    /// the object and the offset held across it.
    pub(crate) fn unset_dimension(
        &mut self,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        self.array_access(object)?;
        let served = self.with_array_access(object, *offset, |req, offset| {
            crate::api::call::call_method(
                req,
                &object,
                b"offsetUnset",
                std::slice::from_ref(&offset),
            )
        })?;
        self.release(served);
        Ok(())
    }

    /// `zend_std_get_constructor`: the class's `__construct` where the
    /// fake or the executed scope may reach it, `zend_bad_constructor_call`'s
    /// `Error` where it may not, the object then marked as one whose
    /// constructor failed so no destructor is served for it
    /// (`zend_object_store_ctor_failed`), and none for a class with no
    /// constructor.
    ///
    /// ```php
    /// class P { private function __construct() {} function __destruct() { echo "destructed\n"; } }
    /// try { new P; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Call to private P::__construct() from global scope
    /// ```
    pub(crate) fn constructor_of(&mut self, object: PhpObject) -> Result<Option<Function>, Thrown> {
        let Some(constructor) = self.method_named(object.class(), b"__construct") else {
            return Ok(None);
        };
        let scope = self.property_scope();
        if self.method_accessible_from(constructor, scope) {
            return Ok(Some(constructor));
        }
        // `zend_bad_constructor_call`: the sentence names no `method`.
        let shown = visibility_string(self.method_visibility(constructor));
        let class = self
            .method_class(constructor)
            .map_or_else(|| String::from("object"), |class| self.class_name(class));
        let from = match scope {
            Some(scope) => format!("scope {}", self.class_name(scope)),
            None => String::from("global scope"),
        };
        object.heap().set_destructor_called(true);
        Err(crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Call to {shown} {class}::__construct() from {from}"),
        ))
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
                let value = self.unwrap_reference(value);
                if value.as_php_string().is_some() {
                    return Ok(Some(value));
                }
                self.release(value);
                let class = self.class_name(object.class());
                Err(crate::api::raising::throw_error(
                    self,
                    "Error",
                    &format!("Method {class}::__toString() must return a string value"),
                ))
            }
            TypeTag::Bool => Ok(Some(Value::bool(true))),
            _ => Ok(None),
        }
    }

    /// `zend_std_get_debug_info` (`Z/Zend/zend_object_handlers.c:2148`):
    /// what `__debugInfo` returns, an array's rows, the empty set for
    /// `null` and `E_ERROR` for anything else. `None` for a class that
    /// declares none, which is php's `get_properties(object)` there: the
    /// members themselves, left to the reader that asked. The rows the
    /// method returned are the user's own and carry no mangling, so a
    /// row named as a private property is shown under the name the
    /// method gave it.
    ///
    /// ```php
    /// class D { public $a = 1; public function __debugInfo(): array { return ['shown' => 2]; } }
    /// print_r(new D);
    /// var_dump(new D);
    /// ```
    /// ```expect
    /// D Object
    /// (
    ///     [shown] => 2
    /// )
    /// object(D)#1 (1) {
    ///   ["shown"]=>
    ///   int(2)
    /// }
    /// ```
    ///
    /// ```php
    /// class D { private $a = 1; public function __debugInfo(): array { return ['a' => 2, 5]; } }
    /// print_r(new D);
    /// var_dump(new D);
    /// ```
    /// ```expect
    /// D Object
    /// (
    ///     [a] => 2
    ///     [0] => 5
    /// )
    /// object(D)#1 (2) {
    ///   ["a"]=>
    ///   int(2)
    ///   [0]=>
    ///   int(5)
    /// }
    /// ```
    ///
    /// A `null` is an empty table, with php's deprecation since 8.5:
    ///
    /// ```php
    /// class N { public $a = 1; public function __debugInfo() { return null; } }
    /// var_dump(new N);
    /// ```
    /// ```expect
    /// Deprecated: Returning null from N::__debugInfo() is deprecated, return an empty array instead in %s on line 3
    /// object(N)#1 (0) {
    /// }
    /// ```
    ///
    /// ```php
    /// class E { public function __debugInfo() { return 1; } }
    /// var_dump(new E);
    /// ```
    /// ```expect exit=255
    /// Fatal error: __debuginfo() must return an array in %s on line 3
    /// ```
    pub(crate) fn debug_info(&mut self, object: PhpObject) -> Result<Option<PhpArray>, Thrown> {
        if self.method_named(object.class(), b"__debuginfo").is_none() {
            // `zend_lazy_object_debug_info`: a proxy that forwards shows
            // its instance, and a lazy object that waits shows its
            // members as they stand, neither initialised for it.
            return self.lazy_debug_info(object);
        }
        let value = crate::api::call::call_method(self, &object, b"__debugInfo", &[])?;
        let value = self.unwrap_reference(value);
        match value.as_array() {
            // The array the method returned, its one owner the caller's,
            // which is php's `*is_temp = 1` over the same table.
            Some(array) => Ok(Some(*array)),
            None if value.tag() == TypeTag::Null => {
                self.release(value);
                let class = self.class_name(object.class());
                crate::api::raising::deprecated(
                    self,
                    &format!(
                        "Returning null from {class}::__debugInfo() is deprecated, return an empty array instead"
                    ),
                );
                PhpArray::new(self).map(Some)
            }
            None => {
                self.release(value);
                Err(crate::api::raising::fatal(
                    self,
                    "__debuginfo() must return an array",
                ))
            }
        }
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
    ///
    /// ```php
    /// $o = $l = new stdClass; $p = $r = new stdClass;
    /// for ($i = 0; $i < 25000; $i++) { $o->n = new stdClass; $o = $o->n; $p->n = new stdClass; $p = $p->n; }
    /// try { var_dump($l == $r); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// foreach ([$l, $r] as $h) { while ($next = $h->n ?? null) { $h->n = $next->n ?? null; } }
    /// ```
    /// ```expect regime=small-stack
    /// Maximum call stack size reached during object comparison
    /// ```
    pub(crate) fn compare_objects(
        &mut self,
        left: &Value,
        right: &Value,
    ) -> Result<Option<std::cmp::Ordering>, Thrown> {
        use std::cmp::Ordering;
        if self.stack_overflowed() {
            return Err(crate::api::raising::throw_error(
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
        let eager = |o: Obj| o.laziness() == php_heap::Laziness::Eager;
        let order = if x.properties().is_none() && y.properties().is_none() && eager(x) && eager(y)
        {
            self.compare_slots(x, y)
        } else {
            // `zend_std_get_properties_ex` on each side, which
            // initialises a lazy one first, the left before the right.
            self.lazy_init(a).and_then(|a| {
                let b = self.lazy_init(b)?;
                self.compare_tables(a.heap(), b.heap())
            })
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
            return Err(crate::ops::nesting_too_deep(self));
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

    /// `zend_compare_symbol_tables` over the two objects' tables as
    /// `zend_std_get_properties_ex` lays them down
    /// (`rebuild_object_properties_internal`), each in php's positions:
    /// one table compares equal to itself; else the counts first, every
    /// declared slot a row while the slots stand for them and every copy
    /// a row once the table holds copies of them
    /// ([`php_heap::DeclaredRows::Copied`]), then `zend_hash_compare`
    /// with `ordered` off, the left table guarded for the walk: each of
    /// its rows looked up by key in the right one, a key the right one
    /// lacks `1`, an uninitialised slot `-1` against a held row and `1`
    /// the other way, two held ones as `zend_compare` compares them.
    ///
    /// ```php
    /// #[AllowDynamicProperties]
    /// class C { public int $b = 1; function __construct() { $this->a = 1; $this->b = 3; } }
    /// $o = (new ReflectionClass(C::class))->newLazyGhost(function ($o) { $o->__construct(); });
    /// var_dump($o); $o->a += 1;
    /// $q = new C; $q->a = 2; $q->b = 1;
    /// var_dump($o == $q, $q == $o, $o == new C, $o < $q, $o->b);
    /// #[AllowDynamicProperties]
    /// class R { public $r; }
    /// $x = new R; $x->r = $x; $x->d = 1; $y = new R; $y->r = $y; $y->d = 1;
    /// try { var_dump($x == $y); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// lazy ghost object(C)#3 (0) {
    ///   ["b"]=>
    ///   uninitialized(int)
    /// }
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// int(3)
    /// Nesting level too deep - recursive dependency?
    /// ```
    fn compare_tables(&mut self, x: Obj, y: Obj) -> Result<std::cmp::Ordering, Thrown> {
        let left = self.heap.object_property_table(x);
        let left = self.fatal_of(left)?;
        let right = self.heap.object_property_table(y);
        let right = self.fatal_of(right)?;
        let table = left.table();
        if table.is_same(right.table()) {
            return Ok(std::cmp::Ordering::Equal);
        }
        if left.len() != right.len() {
            return Ok(left.len().cmp(&right.len()));
        }
        if table.is_recursive() {
            return Err(crate::ops::nesting_too_deep(self));
        }
        table.protect();
        let order = self.compare_rows(left, right, ClassId(x.class()));
        table.unprotect();
        order
    }

    /// The walk of [`Request::compare_tables`]: each row of `left` in
    /// its order against the row of the same key in `right`, both
    /// objects of `class`, whose slots' mangled names key their
    /// declared rows.
    fn compare_rows(
        &mut self,
        left: php_heap::PropertyTable,
        right: php_heap::PropertyTable,
        class: ClassId,
    ) -> Result<std::cmp::Ordering, Thrown> {
        use std::cmp::Ordering;
        let names = self.slot_names(class)?;
        let key_of = |row: php_heap::TableRow| match row {
            php_heap::TableRow::Entry(key) => Some(key),
            php_heap::TableRow::Slot(i) => usize::try_from(i)
                .ok()
                .and_then(|i| names.get(i))
                .map(|name| php_heap::Key::Str(*name)),
        };
        let mut pos = 0;
        while pos < left.used() {
            let at = left.row_at(pos);
            pos += 1;
            let Some((row, p)) = at else {
                continue;
            };
            let Some(key) = key_of(row) else {
                continue;
            };
            // `zend_hash_find` over the right table: a declared slot by
            // its mangled name while the slots stand for the declared
            // rows, the table's own row otherwise.
            let mut found = None;
            let mut at = 0;
            while let Some((php_heap::TableRow::Slot(i), z)) = right.row_at(at) {
                if key_of(php_heap::TableRow::Slot(i)) == Some(key) {
                    found = Some(z);
                    break;
                }
                at += 1;
            }
            let q = match found {
                Some(z) => z,
                None => match right.table().get(key) {
                    Some(z) => z,
                    None => return Ok(Ordering::Greater),
                },
            };
            let order = match (p.is_undef(), q.is_undef()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => {
                    crate::ops::compare(self, &Value::of_zval(p), &Value::of_zval(q))?
                }
            };
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(Ordering::Equal)
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

    pub(crate) fn precision(&self) -> i32 {
        self.ini_number(b"precision", 14)
    }

    /// `short_open_tag`, php's `CG(short_tags)`: whether a `<?` opens
    /// code in the source this request compiles, which its scanner reads
    /// at every tag it meets (`Z/Zend/zend_language_scanner.l:2331`).
    /// php's built-in default is on (`DEFAULT_SHORT_OPEN_TAG`), which is
    /// what an engine with no entry for it answers, and the entry is
    /// `PHP_INI_SYSTEM|PHP_INI_PERDIR`, so no `ini_set()` of the
    /// request's moves it. `token_get_all` reads the same entry, which
    /// is why it is the request's and not the engine's alone.
    pub(crate) fn short_open_tag(&self) -> bool {
        self.ini_get(b"short_open_tag")
            .is_none_or(Request::ini_parse_bool)
    }

    /// `EG(assertions)`, php's `zend.assertions` read as
    /// `OnUpdateAssertions` reads it (`zend_ini_parse_quantity_warn`):
    /// `1` compiles and runs an assertion, `0` compiles it behind an
    /// `ASSERT_CHECK` that jumps over it, and `-1` compiles nothing of it.
    pub(crate) fn assertions(&self) -> i64 {
        self.ini_get(b"zend.assertions")
            .map_or(1, Request::ini_parse_bytes)
    }

    /// `OnUpdateAssertions` (`Z/Zend/zend.c:156-172`) for an
    /// `ini_set()`: a value that moves the entry to or from a negative
    /// one is php's warning and a refusal that keeps the entry as it
    /// was, since what is compiled already was compiled under it; any
    /// other value is taken.
    ///
    /// ```php
    /// var_dump(ini_get('zend.assertions'), ini_set('zend.assertions', '-1'), ini_set('zend.assertions', '0'), ini_get('zend.assertions'), ini_set('zend.assertions', '1'));
    /// ```
    /// ```expect
    /// Warning: zend.assertions may be completely enabled or disabled only in php.ini in %s on line 2
    /// string(1) "1"
    /// bool(false)
    /// string(1) "1"
    /// string(1) "0"
    /// string(1) "0"
    /// ```
    fn assertions_updated(&mut self, value: &[u8]) -> bool {
        let (current, next) = (self.assertions(), Request::ini_parse_bytes(value));
        if current != next && (current < 0 || next < 0) {
            crate::api::raising::warning(
                self,
                "zend.assertions may be completely enabled or disabled only in php.ini",
            );
            return false;
        }
        true
    }

    /// `serialize_precision`, what `var_dump`, `var_export`, `serialize`
    /// and `json_encode` print a float at; -1, its default, asks for the
    /// shortest text that reads back as the same double.
    #[must_use]
    pub fn serialize_precision(&self) -> i32 {
        self.ini_number(b"serialize_precision", -1)
    }

    /// The request headers the host received, `getallheaders()`, as
    /// strings of the request's.
    pub fn request_headers(&mut self) -> Result<PhpArray, Thrown> {
        let mut array = PhpArray::new(self)?;
        let headers = self
            .description
            .web
            .as_ref()
            .map(|web| web.headers.clone())
            .unwrap_or_default();
        for (name, value) in &headers {
            let key = Key::Str(PhpString::new(self, name)?);
            let value = Value::string(PhpString::new(self, value)?);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// `sapi_get_request_time`, when the request started, as `$_SERVER`
    /// and opcache's `ZCG(request_time)` read it.
    ///
    /// ```php
    /// $t = $_SERVER['REQUEST_TIME_FLOAT'];
    /// usleep(10000);
    /// var_dump($t <= microtime(true), (int) $t === $_SERVER['REQUEST_TIME'], $_SERVER['REQUEST_TIME'] <= time());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// ```
    pub(crate) fn started(&self) -> std::time::Duration {
        self.started
    }

    /// `ZCG(request_time)` and `sapi_get_request_time()` as a `zend_long`:
    /// when the request started, in whole seconds.
    #[must_use]
    pub fn request_time(&self) -> i64 {
        i64::try_from(self.started.as_secs()).unwrap_or(i64::MAX)
    }

    // -------------------------------------------------- diagnostics

    // -------------------------------------------------- symbols

    /// `PHP_CLI_PROCESS_TITLE` as the host registers it.
    pub(crate) fn process_title(&self) -> crate::engine::ProcessTitle {
        self.description.process_title
    }

    // -------------------------------------------------- state

    // -------------------------------------------------- environment

    /// The slot for a throwable PHP code served by a stream's kind, a
    /// filter or a user-space wrapper raised inside an operation whose
    /// answer has no room for it: php's `EG(exception)` standing until
    /// the native that asked for the operation returns, which raises it,
    /// and before which no more such code is served.
    pub fn stream_thrown(&mut self) -> &mut Option<Thrown> {
        &mut self.stream_thrown
    }

    /// php's `EG(persistent_list)`, the engine's pool of what the stream
    /// layer keeps between requests ([`crate::api::abi::PersistentList`]).
    #[must_use]
    pub fn persistent_list(&self) -> Arc<crate::api::abi::PersistentList> {
        Arc::clone(&self.shared.persistent)
    }

    /// The heap's memory limit in bytes, `PG(memory_limit)` as
    /// `zend_set_memory_limit` set it.
    pub fn heap_memory_limit(&self) -> usize {
        self.heap.memory_limit()
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

/// Where a walk over an object stands at a property: a declared slot,
/// the key of a bucket of the dynamic table, a hooked property's value
/// as its `get` hook answered it, or a row the class's `get_properties`
/// answered that the object holds nowhere of its own; each of the last
/// two one owner the walker's to give up.
#[derive(Clone, Copy)]
pub(crate) enum Walked {
    Slot(u32),
    Dynamic(php_heap::Key),
    Hooked(Value),
    Answered(Value),
}

impl Walked {
    /// Whether the walk holds an owner of the value it found.
    pub(crate) fn owned(self) -> bool {
        matches!(self, Walked::Hooked(_) | Walked::Answered(_))
    }
}

/// One `zend_property_info *` of a reference's source list
/// (`Z/Zend/zend_types.h:626`): a typed property whose slot holds the
/// cell, named by the class that declared it, the name the `TypeError`
/// prints and the declared type the write is held to. php names the
/// declaration itself; the linked tables here are shared and immutable,
/// so the row carries what the check and the message read.
pub(crate) struct RefSource {
    /// The class that declared the property, which is the one php's
    /// `prop->ce->name` prints.
    pub(crate) declaring: ClassId,
    /// The property's name, unmangled, as php's
    /// `zend_get_unmangled_property_name` hands it over.
    pub(crate) name: Vec<u8>,
    /// `prop_info->type`.
    pub(crate) ty: php_unit::decl::Type,
}

impl HeapHooks for Hooks<'_> {
    fn heap(&mut self) -> &mut Heap {
        &mut self.0.heap
    }

    /// The pointer through the links the bundle supplied, where it
    /// supplied any.
    fn gc_collect_cycles(&mut self) -> Option<i64> {
        let linked = !self.0.shared.supplied.gc_collect.links().is_empty();
        linked.then(|| self.0.gc_collect())
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
        self.0.free_beside(PhpObject::of(obj));
    }

    /// php's `dtor_obj != zend_objects_destroy_object || ce->destructor`:
    /// the class declares or inherits `__destruct`, or its handler
    /// table serves a destructor of its own, which `Fiber`'s and
    /// `Generator`'s do. A generator the collector finds is destroyed
    /// before it is freed, its `finally` blocks served, and it is freed
    /// by the release of what held it, not counted among what the
    /// collection freed.
    ///
    /// ```php
    /// $a = new stdClass; $b = new stdClass; $a->b = $b; $b->a = $a;
    /// $a->gen = (function () { yield 1; })();
    /// unset($a, $b);
    /// var_dump(gc_collect_cycles());
    /// ```
    /// ```expect
    /// int(2)
    /// ```
    /// The destructor phase served in the collector's own fiber while
    /// a fiber runs ([`crate::fiber::gc_destructors_in_fiber`]).
    fn gc_destructors_in_fiber(&mut self, from: u32, end: u32) -> bool {
        crate::fiber::gc_destructors_in_fiber(self.0, from, end)
    }

    fn has_destructor(&mut self, obj: Obj) -> bool {
        let class = ClassId(obj.class());
        self.0.method_named(class, b"__destruct").is_some()
            || crate::fiber::serves_destructor(self.0.object_handlers(obj))
            || crate::generator::is_generator(self.0, PhpObject::of(obj))
    }

    /// The live temporaries of every frame of the running execution,
    /// innermost first (`zend_gc_check_root_tmpvars`): each frame's
    /// `ZEND_LIVE_TMPVAR` and `ZEND_LIVE_LOOP` ranges open at the opline
    /// it stands at, a range that starts past it ending the walk of that
    /// frame's table, as php's sorted table does. A `foreach` by
    /// reference that a collection ran inside holds its array in a live
    /// `LOOP` temporary, which the collection roots again after it, and
    /// the array's cycles are found by the next one.
    ///
    /// ```php
    /// $a = [0, 1];
    /// foreach ($a as &$v) {
    ///     $a[0] =& $a;
    ///     $a[1] = array();
    ///     $a[1][0] =& $a[1];
    ///     $b = 1;
    ///     $a =& $b;
    ///     var_dump(gc_collect_cycles());
    ///     break;
    /// }
    /// var_dump(gc_status()['roots']);
    /// unset($v);
    /// var_dump(gc_collect_cycles());
    /// ```
    /// ```expect
    /// int(0)
    /// int(1)
    /// int(2)
    /// ```
    fn gc_live_temporaries(&mut self, visit: &mut dyn FnMut(Zval)) {
        let req = &*self.0;
        for (at, state) in req.vm.frames().iter().enumerate().rev() {
            let Some(code) = crate::exec::body_of(&state.unit, state.body) else {
                continue;
            };
            let frame = FrameRef(u32::try_from(at).unwrap_or(u32::MAX));
            for range in &code.live_ranges {
                if range.start.0 > state.opline {
                    break;
                }
                if range.end.0 <= state.opline {
                    continue;
                }
                if matches!(
                    range.kind,
                    php_unit::LiveKind::TmpVar | php_unit::LiveKind::Loop
                ) {
                    let slot = crate::exec::temporary_slot(code, range.var);
                    visit(req.slot(frame, slot).to_zval());
                }
            }
        }
    }

    /// A `WeakMap`'s keys beside its entries (`crate::weakrefs::gc_entries`).
    fn gc_weakmap_entries(&mut self, map: Obj, visit: &mut dyn FnMut(Obj, ZvalRef)) -> bool {
        crate::weakrefs::gc_entries(self.0, map, visit)
    }

    /// The `WeakMap` entries keyed by an object
    /// (`crate::weakrefs::gc_keyed`).
    fn gc_weakly_keyed(&mut self, key: Obj, visit: &mut dyn FnMut(ZvalRef, Obj)) {
        crate::weakrefs::gc_keyed(self.0, key, visit);
    }

    /// `get_gc`, through the table: what the payload owns, handed to the
    /// collector's walk as zvals.
    fn gc_children(&mut self, obj: Obj, visit: &mut dyn FnMut(Zval)) {
        let table = self.0.object_handlers(obj);
        table.get_gc(self.0, PhpObject::of(obj), &mut |value| {
            visit(value.to_zval());
        });
        // `zend_lazy_object_get_gc`: the initializer a lazy object waits
        // for, or the instance a proxy forwards to.
        self.0.lazy_gc(obj, &mut |value| visit(value.to_zval()));
    }

    /// `list_dtor_ex`: the type's destructor as registered, handed the
    /// resource with the type it had, as php hands it a copy of the
    /// struct; a type with no destructor closes silently.
    fn free_resource(&mut self, res: Res, kind: ResourceType) {
        if let Some(dtor) = self.0.resources.dtor(kind) {
            dtor(self.0, Resource::of(res));
        }
        // `zend_close_rsrc_list`'s `zend_catch`: what a destructor bailed
        // out of is caught behind it, and the walk goes on.
        if self.0.phase() == Phase::ResourceShutdown {
            self.0.catch_bailout();
        }
    }

    fn out_of_memory(&mut self, bytes: usize) -> Fatal {
        Fatal::OutOfMemory(bytes)
    }

    /// `zend_free_source_list`: the row the cell's word names, dropped
    /// with the cell. The heap has already read the word, so nothing
    /// here reads the reference again.
    fn free_reference(&mut self, r: Ref) {
        let word = r.sources();
        self.0.ref_sources.retain(|(at, _)| *at != word);
    }

    /// `ZEND_REF_DEL_TYPE_SOURCE` for a slot of an object being freed
    /// ([`Request::slot_source_gone`]).
    fn slot_source_gone(&mut self, obj: Obj, slot: u32, r: Ref) {
        self.0.slot_source_gone(PhpObject::of(obj), slot, r);
    }

    /// `zend_weakrefs_notify`: every weak holder of the object cleared
    /// before its members go (`crate::weakrefs::notify`).
    fn weakrefs_notify(&mut self, obj: Obj) {
        crate::weakrefs::notify(self.0, obj);
    }

    /// `zend_lazy_object_del_info` as a lazy object is freed
    /// ([`Request::lazy_forget`]).
    fn forget_lazy(&mut self, obj: Obj) {
        self.0.lazy_forget(obj);
    }
}

#[cfg(test)]
impl Request {
    /// [`Request::push_frame`] in a test, whose request no memory limit
    /// holds, so the push is never refused.
    pub(crate) fn pushed(&mut self, unit: Arc<Unit>) -> FrameRef {
        let pushed = self.push_frame(unit);
        assert_eq!(pushed.as_ref().err(), None, "a test's frame is pushed");
        pushed.unwrap_or(FrameRef(0))
    }
}

/// A host's value made an array of `req`'s: a map's keys strings, a
/// list's the next index ([`Request::host_status`]).
fn host_value(req: &mut Request, value: crate::engine::HostValue) -> Result<PhpArray, Thrown> {
    use crate::engine::HostValue;
    let mut array = PhpArray::new(req)?;
    let rows: Vec<(Option<Vec<u8>>, HostValue)> = match value {
        HostValue::Map(rows) => rows.into_iter().map(|(k, v)| (Some(k), v)).collect(),
        HostValue::List(rows) => rows.into_iter().map(|v| (None, v)).collect(),
        other => vec![(None, other)],
    };
    for (key, row) in rows {
        let value = match row {
            HostValue::Int(n) => Value::int(n),
            HostValue::Float(f) => Value::float(f),
            HostValue::Str(s) => Value::string(PhpString::new(req, &s)?),
            nested => Value::array(host_value(req, nested)?),
        };
        match key {
            Some(key) => {
                let key = Key::from_string(PhpString::new(req, &key)?);
                array.set(req, key, value)?;
            }
            None => array.append(req, value)?,
        }
    }
    Ok(array)
}

/// Whether a row's key is the declared name `name`: a string key of its
/// bytes, never an integer one.
fn key_names(key: &Key, name: &[u8]) -> bool {
    matches!(key, Key::Str(s) if s.as_bytes() == name)
}

#[cfg(test)]
mod tests {
    use super::{Level, Request};
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
            implementors: Vec::new(),
            aliases: Vec::new(),
            traits: Vec::new(),
            adaptations: Vec::new(),
            backing: EnumBacking::Pure,
            constants: Vec::new(),
            properties: Vec::new(),
            methods: vec![MethodDecl {
                function: FunctionDecl {
                    name: Name(b"__destruct".as_slice().into()),
                    parameters: Vec::new(),
                    return_type: None,
                    returns_by_ref: false,
                    is_generator: false,
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
            object_size: 0,
            object_beside: Box::default(),
        }
    }

    /// The entry of the `__destruct` above: the branch under test
    /// refuses the call, so a raise here is the test failing.
    fn never_served(req: &mut Request) -> Result<crate::api::values::Value, super::Thrown> {
        Err(crate::api::raising::throw_error(
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
        let engine = Engine::cli_builder().extensions(&[destructible]).build();

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
        let engine = Engine::cli_builder().extensions(&[destructible]).build();
        let mut request = request(&engine);
        let Ok(object) = request.new_object_of("Destructible") else {
            return;
        };
        request.raised = Some(super::Raised::Throwable(object));

        request.destruct(object);

        assert!(
            request.pending().is_some(),
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
        let engine = Engine::cli_builder().ini("error_reporting", "0").build();
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
        let engine = Engine::cli_builder().ini("html_errors", "1").build();
        let mut req = request(&engine);
        req.note_error_in(Level::UserError, "b<o>om", file, 2);
        req.note_error_in(Level::Error, "b<o>om", file, 3);
        assert_eq!(
            String::from_utf8_lossy(&req.take_body()),
            "<br />\n<b>Fatal error</b>:  b<o>om in <b>witness.php</b> on line <b>2</b><br />\nStack trace:\n#0 {main}\n\
             <br />\n<b>Fatal error</b>:  b&lt;o&gt;om in <b>witness.php</b> on line <b>3</b><br />\n"
        );

        let engine = Engine::cli_builder()
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
        let engine = Engine::cli_builder()
            .ini("log_errors", "1")
            .ini("xmlrpc_errors", "1")
            .build();
        let mut req = request(&engine);
        req.note_error_in(Level::CompileError, "refused", file, 2);
        assert_eq!(
            req.take_aside(),
            vec![super::response::Aside::Log(
                b"PHP Fatal error:  refused in witness.php on line 2\nStack trace:\n#0 {main}"
                    .to_vec()
            )]
        );
        assert_eq!(
            String::from_utf8_lossy(&req.take_body()),
            "<?xml version=\"1.0\"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>0</int></value></member><member><name>faultString</name><value><string>Fatal error:refused in witness.php on line 2\nStack trace:\n#0 {main}</string></value></member></struct></value></fault></methodResponse>"
        );
    }
}
