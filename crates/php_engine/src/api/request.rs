//! The request-environment group (design §4.4, §5.14): what a native
//! reaches through `&mut Request`, the one parameter that says a function
//! may re-enter PHP code (principle 9, ADR-0002), `api.lock`'s largest
//! group. The skeleton answers each from what the request holds; the
//! body bytes and the header block are kept on the request and handed to
//! the sink by `serve`, which owns the commit moment; `serve` says what
//! the request came to.
//!
//! Each area of the request stands in a file of its own under
//! `api/request/`, and the state an area alone reads is a struct whose
//! fields are private to that file; this file holds the request itself,
//! what more than one area reads, and its making.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use php_heap::{Fatal, FrameRef, Heap, HeapHooks, Obj, Ref, Res, ResourceType, Zval, ZvalRef};
use php_link::LinkedClass;
use php_unit::PublishedUnit;
use php_unit::decl::{ClassDecl, FunctionDecl, HookKind, Visibility};

use crate::api::abi::GcTable;
use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, PhpObject, Resource, Value};
pub use crate::engine::Standard;
use crate::engine::{OutputSink, RequestDescription, Shared};
use crate::exec::{self, Vm};
pub use crate::open_basedir::{HostPath, Refused};
use crate::output::Output;
pub use crate::virtual_cwd::{MAXPATHLEN, Resolve};
pub(crate) use callable::{Deprecations, Refusal, compound_split};
pub use execution::Execution;
/// `zend_call_stack`: a thread's machine stack as the platform describes
/// it, its `base` and `max_size` ([`Limits::call_stack_bounds`]).
pub use php_heap::CallStack;
pub use unit_cache::{
    CacheBudget, CacheStatus, CachedFile, Invalidation, PreloadStatistics, Restart, TierStatus,
    UnitCache,
};

mod autoload;
mod build_defs;
pub use build_defs::BuildDefs;
#[cfg(php_build_provider)]
pub(crate) use build_defs::PROVIDER as BUILD_PROVIDER;
pub(crate) use build_defs::build_date;
mod callable;
mod caller;
pub use caller::Frames;
pub(crate) use caller::NativeCaller;
pub use hooks::HookedTable;
pub(crate) use hooks::{HookedBuild, HookedDeclared, HookedRows};
pub(crate) use objects::placed_of;
mod calls;
pub use calls::Supplier;
pub(crate) use calls::{Callee, Family, Retval};
mod class_constants;
pub(crate) use class_constants::{ByValue, ConstantFetch, ConstantRead};
mod class_data;
pub(crate) use class_data::PropertyNames;
mod classes;
use classes::AccelServed;
pub use classes::Symbols;
mod constants;
mod declared_bytes;
pub(crate) use constants::{ConstantAt, halt_offset_key};
mod diagnostics;
pub use diagnostics::Diagnostics;
use diagnostics::{DISPLAY_ERRORS_STDERR, display_errors_mode_of, fatal_errors};
pub use diagnostics::{FormatDate, Syslog};
pub(crate) use diagnostics::{append_escaped, escape_html};
pub(crate) mod ending;
pub(crate) use ending::{Ending, Phase};
mod environment;
pub use environment::Environment;
mod execution;
mod extensions;
pub use extensions::Extensions;
pub use extensions::InfoRow;
mod foreign;
pub use foreign::{Foreign, ForeignId, Held};
mod frame_reads;
mod frame_writes;
pub(crate) use frame_reads::Identity;
pub use frame_reads::{ChainFrame, FrameId, FrameView};
mod frames;
pub(crate) use frames::CodeFrom;
use frames::{Expand, Link, Slots, links_of};
mod include;
pub use include::Code;
mod ini;
pub use ini::Ini;
pub(crate) mod ini_files;
pub use ini::IniEntry;
pub(crate) use ini::{CoreQuantity, TypedIni};
pub use ini_files::IniFiles;
mod hold;
mod hooks;
mod inspect;
mod lazy;
mod limits;
pub use limits::Limits;
pub use limits::{Interrupt, TimeLimitSuspended};
mod linking;
mod load_file;
mod memory;
pub(crate) use memory::Pushed;
pub use memory::{Allocation, AllocationObserverId, Memory, ObserveAllocations};
pub use memory::{StandaloneHeap, ecalloc, efree, emalloc, erealloc};
mod methods;
use methods::visibility_string;
pub use methods::{MethodRow, PropertyInfoRow};
mod objects;
mod observing;
mod oom;
mod op_arrays;
pub(crate) use observing::{ObservedChain, OpenCall};
pub use op_arrays::{ConstIdx, OpArrayView, Opcode, Operand, OplineIdx, OplineView};
pub(crate) mod paths;
pub use paths::RealpathEntry;
pub(crate) use paths::{is_cacheable_stream_path, is_wrapper_url};
mod pending;
pub(crate) use pending::Leaving;
pub use pending::Raised;
mod post;
mod preload;
mod properties;
mod publish;
use properties::IN_GET;
pub(crate) use properties::{Accessor, ReadWriteSlot, RefSource, TypedProperty, Walked};
mod recording;
mod resources;
pub use resources::{ResourceDtor, Resources};
mod response;
pub use response::Response;
#[cfg(test)]
pub(crate) use response::{Aside, Kept};
mod rooms;
pub(crate) mod ropes;
#[cfg(debug_assertions)]
mod sensing;
pub use crate::output::HttpStatus;
pub use response::{
    BufferHandler, BufferInit, ContentType, HandlerCall, HandlerOp, HandlerStatus, OutputOp,
    StartHandler,
};
mod shutdown;
mod site_override;
mod state;
pub(crate) use state::StateSlots;
mod static_props;
mod static_vars;
mod symbol_tables;
pub use symbol_tables::Overwrite;
mod table;
#[cfg(debug_assertions)]
mod teardown;
pub(crate) mod temporary;
mod ticks;
mod traces;
mod unit_cache;
mod unit_strings;
pub(crate) use unit_cache::HandlePath;
mod variables;
pub use variables::Variables;
mod zone;
pub use post::{ReadPost, Upload};
pub use variables::url_decode;
pub use variables::{BodyLimits, Input};
pub use zone::Zone;

pub(crate) use hooks::parent_hook_trampolines;
pub(crate) use lazy::{RawWrite, Strategy};
pub(crate) use preload::{Keyed, Named, Preloaded, Preloading};

/// php's version, what `PHP_VERSION` and `phpversion()` say.
pub const PHP_VERSION: &str = "8.5.12";

/// A linked extension, by identity: its `module_number`, where its
/// `zend_module_entry` stands in `module_registry`: `Core` zero, as
/// `zend_startup` registers it first, and each extension of the bundle
/// the next in startup order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExtensionId(pub(crate) u32);

impl ExtensionId {
    /// `Core`, the engine's own natives, classes, constants and ini.
    pub(crate) const CORE: ExtensionId = ExtensionId(0);

    /// The extension at `at` in `module_registry`.
    pub(crate) fn at(at: usize) -> ExtensionId {
        ExtensionId(u32::try_from(at).unwrap_or(u32::MAX))
    }

    /// The origin a debug build's heap tags what it allocates for this
    /// extension with ([`php_heap::Origin`]): its number.
    #[cfg(debug_assertions)]
    pub(crate) fn origin(self) -> php_heap::Origin {
        php_heap::Origin::new(u16::try_from(self.0).unwrap_or(u16::MAX))
    }
}

pub use crate::interrupt::Periodic;

/// The process's signal record (design §5.8, revision 38), for the
/// extension that installs signal handlers: [`SIGNAL_POST`] is what its
/// `src/ffi/` handler posts a [`Siginfo`] through, [`take_signals`] what
/// its dispatch takes, and [`Interrupt::receive_signals`] how its
/// asynchronous delivery raises this request's interrupt. The record is
/// the engine's, since signal dispositions are the process's and an
/// extension holds no atomic (ADR-0005). Beside it, [`Reentry`] is how a
/// function C calls back reaches its request ([`Limits::reentry`]):
/// [`Lent`] while the request waits in a call into C or blocks
/// ([`Limits::blocking`]), and otherwise a [`Deferred`] call or one a
/// thread waits on ([`Waited`]) that the interrupt check serves.
pub use crate::signal::{
    DEFERRED_WORDS, Deferred, Lent, Receiving, Reentry, SIGNAL_POST, Siginfo, Waited, take_signals,
};

/// Which side of [`Limits::fork`] the caller stands on, and the child's
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
    php_process::select_fds(sets, timeout)
}

/// Why [`Code::resolve_path`] found no file: the last attempt's
/// reason, or the throwable a wrapper's `url_stat` raised.
#[derive(Debug)]
pub enum Unresolved {
    /// No file: the reason the last attempt failed with.
    Missing(std::io::Error),
    /// What a wrapper's `url_stat` threw, which ends the walk.
    Threw(Thrown),
}

/// One request (design §5.14): `Send`, single-threaded, dropped at the
/// end of `serve`. Everything a native may touch is a method here or on
/// a value it hands out.
#[allow(clippy::struct_excessive_bools)] // php's EG and PG flags, each its own global, read alone
pub struct Request {
    pub(crate) shared: Arc<Shared>,
    /// The request's tier, its loops and its tier's state, laid with the
    /// request and written again only by the hand-off
    /// ([`Request::hand_off`]): private to `api::request`, where the
    /// request is made, and read elsewhere through
    /// [`Request::tier_state`] alone, so a `&mut` of the field or a write
    /// over it anywhere else fails to compile; a write over the whole
    /// request does not ([`crate::tier::TierState`]).
    tier: crate::tier::TierState,
    /// php's `JIT_G` as the request holds it, its engine's start's
    /// rewritten by its `ini_set('opcache.jit', ...)`
    /// ([`UnitCache::tier_setting`](unit_cache::UnitCache::tier_setting)).
    tier_setting: TierStatus,
    /// The units the loop's servings held, kept with their shares for the
    /// next serving ([`crate::tier::UnitsLeft`]).
    pub(crate) units_left: crate::tier::UnitsLeft,
    description: RequestDescription,
    /// The uncaught throwable kept for a host that renders it ([`inspect::Inspected`]).
    inspect: inspect::Inspected,
    /// The calls' state ([`calls::Calls`]).
    calls: calls::Calls,
    /// The VM stack the request runs on and its frames (design §5.6).
    vm: Vm,
    /// What the frames own beyond their headers, every stack's
    /// ([`crate::exec::Annex`]).
    annexes: crate::exec::Annexes,
    /// The map table of design §5.14, php's `CG(map_ptr_base)`: what a
    /// published unit's map pointers index, laid down with the request,
    /// every entry `NULL` until touched, and beside it the caches of the
    /// units compiled for the request alone, php's heap caches. Every op
    /// array's runtime cache stands in one of them
    /// ([`exec::RuntimeCaches`]) and nowhere else.
    map: crate::exec::RuntimeCaches,
    /// How many observers the engine declared (`Shared::observers`),
    /// held on the request, so the observer's homes test it with one load
    /// (design §4.3 seam 9).
    observers: u32,
    /// The extensions' request states ([`state::RequestState`]).
    state: state::RequestState,
    /// The entries the request changed, their bytes, the host's locks
    /// and the stage ([`ini::IniState`]).
    ini: ini::IniState,
    /// The sink, the `php_output` layer, the header callback and the
    /// connection's status ([`response::ResponseState`]).
    response: response::ResponseState,
    /// The diagnostics' state ([`diagnostics::DiagnosticsState`]).
    diagnostics: diagnostics::DiagnosticsState,
    ending: Option<Ending>,
    /// The pieces of the ropes the frames are building ([`ropes::Ropes`]).
    ropes: ropes::Ropes,
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
    /// Each auto-global not made yet ([`variables::VariablesState`]).
    variables: variables::VariablesState,
    /// The symbol tables, [`TableId`] naming one: php's
    /// `EG(symbol_table)` first, the main op array's variables (design
    /// §5.6, §5.10), and after it one table per function frame that
    /// included a file, `zend_rebuild_symbol_table`'s, which goes with
    /// its frame. A table's names stand in the order they were attached
    /// or written, each once; while a frame is attached to a name the
    /// bucket is an `IS_INDIRECT` to the frame's CV ([`SymbolTable`]) and
    /// the frame owns the value, and `zend_detach_symbol_table` moves the
    /// value into the bucket when the frame leaves. The first table
    /// outlives the main frame, so a variable the program wrote at the
    /// top level is still an owner while the shutdown functions are
    /// served; `shutdown_destructors` walks it in reverse before the
    /// store's walk, and `zend_shutdown_executor_values` destroys it
    /// after the extensions' `rshutdown`. `$GLOBALS`, `compact`,
    /// `extract` and `get_defined_vars` read these tables (design §5.6).
    tables: Vec<SymbolTable>,
    /// `EG(symtable_cache)`: the rebuilt tables a frame's end cleaned and
    /// kept for the next rebuild, at most `SYMTABLE_CACHE_SIZE`
    /// ([`Request::rebuild_symbol_table`]).
    symtable_cache: Vec<php_heap::Arr>,
    /// `EG(included_files)`: the resolved path of every file compiled by
    /// the request, the primary one first (`php_execute_script_ex`),
    /// then each an `INCLUDE_OR_EVAL` opened, which the once forms read
    /// before they open one (`zend_include_or_eval`), on the request's
    /// heap ([`include::IncludedFiles`]).
    included_files: include::IncludedFiles,
    /// The request's constants and `PHP_SAPI` ([`constants::Constants`]).
    constants: constants::Constants,
    /// The class constants' state ([`class_constants::ClassConstants`]).
    class_constants: class_constants::ClassConstants,
    /// The natives the classes the request declared lay under their own
    /// scope ([`crate::api::call::ClassNative`]): an enum's own methods
    /// and the copies of a native trait's, one function number each of
    /// the request's own from [`Function::CLASS_NATIVE`] on, as php
    /// allocates a `zend_internal_function` per method per class. A
    /// published class's are the arena's.
    class_natives: Vec<(ClassId, crate::api::call::ClassNative)>,
    /// The properties' guards, source lists and scope
    /// ([`properties::Properties`]).
    properties: properties::Properties,
    /// `EG(filename_override)` and `EG(lineno_override)`, set while a
    /// constant expression with a class for its scope is evaluated
    /// ([`site_override`]).
    site_override: Option<site_override::SiteOverride>,
    /// The static properties' slots ([`static_props::StaticProps`]).
    static_props: static_props::StaticProps,
    /// Each class's mutable data and the blocks made beside each object ([`class_data::ClassDataTables`]).
    class_data: class_data::ClassDataTables,
    /// php's `EG(weakrefs)`: every weakly referenced object's id against
    /// the `WeakReference` and `WeakMap` objects that name it
    /// (`crate::weakrefs`).
    weakrefs: crate::weakrefs::Table,
    /// php's `EG(lazy_objects_store)`: what goes with each lazy object's
    /// state, by its id ([`lazy`]).
    lazy: lazy::Table,
    /// Which include is being opened ([`include::Include`]).
    include: include::Include,
    /// A throwable PHP code served by a stream's kind or filter raised
    /// inside an operation whose answer has no room for it: php's
    /// `EG(exception)` standing until the native that asked for the
    /// operation returns, which is when [`Request::serve_native`] raises
    /// it, and before which no more such code is served.
    pub(crate) stream_thrown: Option<Thrown>,
    /// `BG(user_shutdown_function_names)` ([`shutdown::ShutdownFunctions`]).
    shutdown_functions: shutdown::ShutdownFunctions,
    /// `BG(user_tick_functions)` and `EG(ticks_count)` ([`ticks::Ticks`]).
    ticks: ticks::Ticks,
    /// The autoloaders and `EG(in_autoload)` ([`autoload::Autoload`]).
    autoload: autoload::Autoload,
    /// The umask, the locale names and the start time
    /// ([`environment::EnvironmentState`]).
    environment: environment::EnvironmentState,
    cwd: PathBuf,
    /// The fibers running on this request, innermost last, each with the
    /// execution state of the stack that resumed it, and where each
    /// suspends to (design §5.11, `crate::fiber`).
    fibers: crate::fiber::Fibers,
    /// php's `EG(exception)`: what a handler left pending, a throwable or
    /// one of php's two exits ([`Raised`]), which the loop reads at the
    /// `Threw` edge.
    raised: Option<Raised>,
    /// The throwable [`Request::while_pending`]'s step stands in
    /// `raised` for, which the slot names and the step's caller owns.
    stepping_past: Option<PhpObject>,
    /// php's `zend_bailout` under way: a fatal was rendered where it was
    /// raised and the request is leaving for the `zend_try` that catches
    /// it, the outermost serving's end or the step of
    /// `php_request_shutdown` it was raised in ([`Request::catch_bailout`]).
    /// Nothing is served until then: the loop unwinds as it unwinds the
    /// unwind exit, past every `catch` and `finally`, a native reading
    /// what its callback left reads the fatal, and a second fatal raised
    /// on the way is not rendered.
    bailout: bool,
    /// The open observed frames ([`observing::Observing`]).
    observing: observing::Observing,
    /// The frame identities given so far ([`frame_reads::FrameIds`]).
    frame_ids: frame_reads::FrameIds,
    /// Where the request stands ([`ending::Progress`]).
    progress: ending::Progress,
    /// Where the destructors' walk was cut short ([`objects::Objects`]).
    objects: objects::Objects,
    /// What the entry frame of the innermost serving returned.
    returned: Value,
    /// The interrupt, the timer and the stack limit ([`limits::LimitsState`]).
    limits: limits::LimitsState,
    /// What a debug build's sensors have made of the request
    /// ([`sensing::Sensing`]).
    #[cfg(debug_assertions)]
    sensing: sensing::Sensing,
    /// The table the last `FETCH_DIM_W` laid its `IS_INDIRECT` into ([`hold::Holding`]).
    hold: hold::Holding,
    /// The buffers a backtrace's walk lays its links and rows in, kept
    /// between walks ([`traces::Scratch`]).
    traces: traces::Scratch,
    /// The objects of foreign libraries the request keeps for the
    /// extensions, dropped with it where no owner gave them up before
    /// ([`foreign::Table`]).
    foreign: foreign::Table,
    /// php's `EG(class_table)` past what the engine registered at
    /// startup, the aliases aside: each class under its lowered name,
    /// in the order declared, the preload's first and then the
    /// request's, each in the bucket it stands in.
    declared: Keyed<Named>,
    /// The classes the units this request served declared, each the
    /// unit that holds it and the index of its declaration, in the
    /// order declared: the rows a `ClassId` of the request's own counts
    /// into (design §5.14, *Identities*), dropped with the request. A
    /// published class's row is the engine's arena's.
    own_classes: Vec<Declared>,
    /// The engine's published arena of the epoch the request started in
    /// (design §5.14), which every `ClassId` and `Function` of the
    /// published space this request holds names a row of, the preload's
    /// below its own, and whose unit cache files it reads and fills.
    arena: Arc<crate::arena::Arena>,
    /// The classes a load bound early ([`classes::Classes`]).
    classes: classes::Classes,
    /// The classes whose link is under way and what their links left
    /// for later ([`linking::Links`]).
    links: linking::Links,
    /// `EG(record_errors)` and `EG(errors)`: the diagnostics held back
    /// for a compile or a link to emit ([`recording::Record`]).
    record: recording::Record,
    /// `CG(rtd_key_counter)` over the units this request compiled for
    /// itself and every refused compile it raised: the runtime definition
    /// keys and anonymous class names they made, which the next one
    /// counts on from ([`crate::engine::Shared::compile_for`],
    /// [`Request::declare_refused`]).
    keys_made: u32,
    /// The arena every compile this request makes parses into and keeps
    /// its temporaries in, reset after each (#2810): what php's
    /// `CG(ast_arena)` is to one compile, its room kept for the next, so
    /// an `eval` of a statement allocates nothing for its tree. Not the
    /// engine's published arena of rows, the `arena` above.
    compile_arena: php_compile::syntax::Arena,
    /// The last request-only unit the request gave up
    /// ([`Request::give_back`]), whose main op array's tables the next
    /// compile lays its own down in and in whose room the next unit is
    /// published (#3028): php's `destroy_op_array` gives an `eval`'s op
    /// array back to the request's heap, from which the next `eval`'s is
    /// `emalloc`ed.
    spare_unit: Option<php_unit::SpareUnit>,
    /// Whether the string compiling now is one the host evaluates of its
    /// own, which `zend_eval_stringl` compiles with `CG(compiler_options)`
    /// at `ZEND_COMPILE_DEFAULT_FOR_EVAL`, so without the engine's
    /// extended info ([`Request::host_string`]).
    pub(crate) compiling_host_string: bool,
    /// `EG(no_extensions)`: set while a string the host evaluates of its
    /// own is served, so no Zend extension is told of a statement it
    /// reaches ([`Request::serve_host_string`], [`crate::statement`]).
    pub(crate) no_extensions: bool,
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
    /// The functions, methods and closures the units this request
    /// served declared, each the unit that holds it and which of its op
    /// arrays is the body: a `Function` of the request's own counts into
    /// this list (design §5.6, §5.14).
    user_functions: PinnedRows<UserFunction>,
    /// The callees of the functions no table holds one for, a
    /// class's native and a trampoline, each made the first time a
    /// frame serves it ([`Request::callee_at`]).
    named_callees: std::collections::HashMap<Function, Box<crate::readable::Named>>,
    /// The strings the request-only units it compiled own, their
    /// constants and their names, held while each unit lives
    /// ([`unit_strings::UnitStrings`]).
    unit_strings: unit_strings::UnitStrings,
    /// `CG(loop_var_stack)`'s and `CG(delayed_oplines_stack)`'s elements,
    /// laid down by the first compile of the request that pushed onto
    /// one and kept to its end ([`Request::note_pushed`]).
    compiler_stacks: memory::CompilerStacks,
    /// php's `EG(function_table)` past the natives, each function under
    /// its name: the preload's, published, in the order its table holds
    /// them, then each the request declared, in the order declared. A
    /// closure or a method is never a row, being found through its
    /// object or its class.
    functions: Keyed<Function>,
    /// The `static` variables' tables ([`static_vars::StaticVars`]).
    static_vars: static_vars::StaticVars,
    /// Where the request is the preload's (design §6.3), the files it
    /// compiled into the preload.
    preloading: Option<Preloading>,
    /// The unit opcache last served, `zend_accel_load_script`'s: one it
    /// held or keeps in its file cache, compiled with
    /// `ZEND_COMPILE_DELAYED_BINDING` ([`Request::class_load`]), and
    /// whether this request's miss compiled it; taken by the frame that
    /// runs it, whose `static` table is then the unit's map pointer's
    /// ([`Request::file_statics`]).
    accel_loaded: Option<(PublishedUnit, AccelServed)>,
    /// php's bytes for state the engine keeps typed elsewhere
    /// ([`rooms::Rooms`]).
    rooms: rooms::Rooms,
    /// The unit the cache last served from what it holds, beside its
    /// early-bound classes as they were published with it (design §6.3),
    /// which its load declares by one insert each
    /// ([`Request::declare_classes`]).
    early_bound: Option<(PublishedUnit, crate::unit_cache::EarlyBound)>,
    /// Whether the request's interning asks the engine's table yet,
    /// opcache's `ZCG(counted)` ([`memory::SharedHold`]).
    shared_hold: memory::SharedHold,
    /// The request in flight, which no fork but its own happens beside
    /// (#1807, [`Limits::fork`]): entered before its deadlines are laid
    /// on the engine's timer and left after they are taken out, so no
    /// fork copies the timer's mutex as the request holds it.
    _inside: crate::fork::Inside,
    /// The request's heap, every value of the request a block of it:
    /// declared last, so it is dropped last, after every field that may
    /// hold one. A debug build's owner left in an extension's state after
    /// a fatal reaches its heap through the block it owns as it is
    /// dropped ([`php_heap::owners`]).
    heap: Heap,
}

/// `DEBUG_BACKTRACE_PROVIDE_OBJECT` (`Z/Zend/zend_constants.h`): a
/// method's row carries its receiver.
pub(crate) const DEBUG_BACKTRACE_PROVIDE_OBJECT: i64 = 1;

/// `DEBUG_BACKTRACE_IGNORE_ARGS`: no row carries its arguments.
pub(crate) const DEBUG_BACKTRACE_IGNORE_ARGS: i64 = 2;

/// One symbol table, php's `zend_array` of variables (design §5.6), and
/// the function frame it was rebuilt for, which frees it when it pops;
/// `EG(symbol_table)` has no owner and lives as long as the request. A
/// bucket holds the value the table owns, or an `IS_INDIRECT` to the CV
/// of the frame attached to the name (`zend_attach_symbol_table`), which
/// owns the value while the frame stands, so a fetch by name for writing
/// hands on the bucket's own `zval *` ([`Request::symbol_place`]).
pub(crate) struct SymbolTable {
    owner: Option<FrameRef>,
    rows: Rows,
    /// How many times its buckets were reached to be written, so a frame
    /// that attached to it knows whether they stand as it left them
    /// ([`exec::Annex::attached`]).
    changes: u32,
}

/// Where a symbol table's buckets stand: `EG(symbol_table)`'s header is
/// `EG`'s own, off the request's heap; a rebuilt table is
/// `zend_new_array`'s, on it; and a table freed below one that stands is
/// left in its place with none.
enum Rows {
    Embedded(php_heap::EmbeddedArr),
    Own(php_heap::Arr),
    Gone,
}

/// What `zend_include_or_eval` handed the handler: `ZEND_FAKE_OP_ARRAY`,
/// a once form over a path the table already had, which answers `true`;
/// `NULL`, a path nothing opened or an empty string to `eval`, which
/// answers `false`; or the unit of the file or the string, whose main op
/// array the handler serves on a frame of its own.
pub(crate) enum Opened {
    Already,
    Failed,
    Unit(PublishedUnit),
}

/// What `zend_get_property_offset` places a declared instance property
/// by: the slot it sits in, the visibility a read is held to, the set
/// visibility a write is held to where it has one of its own, and
/// what a write is held to of its type and flags.
#[derive(Debug, Clone, Copy)]
struct DeclaredSlot {
    slot: u32,
    /// `property_info->ce`: the class that declared what stands in the
    /// slot, which is the scope its visibility is measured against.
    declaring: ClassId,
    visibility: Visibility,
    set_visibility: Option<Visibility>,
    /// What a write to the slot is held to.
    ty: crate::exec::SlotType,
    /// `ZEND_ACC_READONLY`, which the set visibility's refusal names.
    readonly: bool,
    /// `property_info->prototype->ce`: the class that first declared the
    /// property up the chain, through every redeclaration that was not
    /// over a private one, which a protected property's access and a
    /// `protected(set)` one's write are measured against
    /// (`is_protected_compatible_scope`); the declaring class for any
    /// other.
    root: ClassId,
}

/// One user function or method a request declared: the unit and the
/// body its op array is read from, and for a method the row of its
/// class's method table it is, from which its declaration is read. The
/// two differ for a method a trait gave a class: php copies the
/// `zend_function` into the class with the class as its scope and
/// shares the trait's opcodes (`zend_add_trait_method`), so the row is
/// the class's and the op array the trait's unit's.
pub(crate) struct UserFunction {
    pub(crate) unit: PublishedUnit,
    pub(crate) body: exec::Body,
    pub(crate) member: Option<Member>,
    /// `ZEND_ACC_DEPRECATED` and `ZEND_ACC_NODISCARD`, which
    /// `zend_compile_func_decl` and the `#[\NoDiscard]` validator raise
    /// from the declaration's attributes, decoded where the function is
    /// declared.
    pub(crate) flags: crate::attributes::Flags,
    /// What every call's frame push and activation read of the
    /// function, read once where it is declared ([`FrameShape`]).
    pub(crate) shape: FrameShape,
    /// The function as another process reads it ([`crate::readable`]):
    /// its name, its file and its oplines, held where the row stands,
    /// which never moves ([`PinnedRows`], the arena's column), so a
    /// frame's header names it by its place.
    pub(crate) record: crate::readable::Record,
}

impl UserFunction {
    /// The row of `body` of `unit`, declared as `decl`: its flags, its
    /// frame's shape and its record read off the declaration and the op
    /// array, where the unit has the body.
    pub(crate) fn of(
        unit: PublishedUnit,
        body: exec::Body,
        member: Option<Member>,
        decl: &FunctionDecl,
        observed: bool,
    ) -> UserFunction {
        let code = exec::body_of(&unit, body);
        let shape = FrameShape::of(decl, code, observed);
        let record = code.map_or_else(crate::readable::Record::default, |code| {
            crate::readable::Record::user(&decl.name.0, code)
        });
        UserFunction {
            flags: crate::attributes::Flags::of(&decl.attributes),
            unit,
            body,
            member,
            shape,
            record,
        }
    }

    /// Where the function's record stands, what a frame serving it
    /// names.
    #[inline]
    pub(crate) fn callee_at(&self) -> crate::readable::CalleeAt {
        self.record.at()
    }
}

/// Rows that never move once pushed: a first segment, then segments of
/// doubling size, each filled no further than the size it was made with,
/// so a row stands where it was pushed for as long as the rows do. What
/// a frame's header names a function's record in
/// ([`UserFunction::callee_at`]). A row of the first segment, nearly
/// every request's every row, is read as a vector's is, one bound and
/// one index, since every call to a function of the request's reads its
/// row.
pub(crate) struct PinnedRows<T> {
    first: Vec<T>,
    later: Vec<Vec<T>>,
    len: usize,
}

impl<T> Default for PinnedRows<T> {
    fn default() -> PinnedRows<T> {
        PinnedRows {
            first: Vec::new(),
            later: Vec::new(),
            len: 0,
        }
    }
}

impl<T> PinnedRows<T> {
    /// The rows the first segment holds; later segment `k` holds this
    /// many `<< (k + 1)`, from `FIRST * ((2 << k) - 1)` on.
    const FIRST: usize = 128;

    /// The later segment and the place in it of row `at`, past the
    /// first segment.
    fn locate(at: usize) -> (usize, usize) {
        let biased = at + Self::FIRST;
        let top = (usize::BITS - 1 - biased.leading_zeros()) as usize;
        let segment = top - Self::FIRST.trailing_zeros() as usize;
        (segment - 1, biased - (Self::FIRST << segment))
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub(crate) fn get(&self, at: usize) -> Option<&T> {
        if let Some(row) = self.first.get(at) {
            return Some(row);
        }
        self.get_later(at)
    }

    #[cold]
    fn get_later(&self, at: usize) -> Option<&T> {
        if at >= self.len || at < Self::FIRST {
            return None;
        }
        let (segment, offset) = Self::locate(at);
        self.later.get(segment)?.get(offset)
    }

    pub(crate) fn push(&mut self, row: T) {
        if self.len < Self::FIRST {
            if self.first.capacity() == 0 {
                self.first.reserve_exact(Self::FIRST);
            }
            self.first.push(row);
            self.len += 1;
            return;
        }
        let (segment, _) = Self::locate(self.len);
        if segment == self.later.len() {
            self.later
                .push(Vec::with_capacity(Self::FIRST << (segment + 1)));
        }
        if let Some(rows) = self.later.get_mut(segment) {
            rows.push(row);
            self.len += 1;
        }
    }

    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.first.iter().chain(self.later.iter().flatten())
    }
}

impl<T> IntoIterator for PinnedRows<T> {
    type Item = T;
    type IntoIter =
        std::iter::Chain<std::vec::IntoIter<T>, std::iter::Flatten<std::vec::IntoIter<Vec<T>>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.first
            .into_iter()
            .chain(self.later.into_iter().flatten())
    }
}

/// What `zend_vm_stack_push_call_frame` and `i_init_func_execute_data`
/// read off a user function (design §5.6): `op_array.num_args` and
/// `ZEND_ACC_HAS_TYPE_HINTS`, how many parameters the declaration has
/// before a variadic and whether any of them declares a type, which
/// place a call's arguments and choose the `RECV*` it skips; and of its
/// op array, `last_var + T`, the room its frame takes, the entry
/// safepoint its prologue runs to, and its runtime cache's map pointer
/// and size. php's compiler writes them all onto the op array; the
/// engine reads them once where the function is declared, so a call
/// reads them off the function table and walks neither the declaration
/// nor the unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FrameShape {
    pub(crate) parameters: u32,
    pub(crate) typed: bool,
    /// The CVs, `last_var`.
    pub(crate) cvs: u32,
    /// The CVs and the temporaries.
    pub(crate) slots: u32,
    /// Whether an observer was registered as the function was linked,
    /// which gives its frame the temporary `pass_two` reserves for the
    /// observers (`op_array->T += ZEND_OBSERVER_ENABLED`,
    /// `Z/Zend/zend_opcode.c:1130`), past its own.
    pub(crate) observed: bool,
    /// The first opline past the `RECV*` prologue, `safepoints[0]`.
    pub(crate) entry: u32,
    /// `ZEND_MAP_PTR(op_array->run_time_cache)`.
    pub(crate) cache: exec::CacheAt,
    /// `op_array.cache_size`, its slots and its room.
    pub(crate) cache_size: exec::CacheSize,
    /// Whether a parameter, the variadic's included, takes its argument
    /// by reference: a call to a function none of whose parameters does
    /// sends every argument by value, which the `SEND_*_EX` oplines read
    /// off the shape and not off the declaration
    /// ([`Request::arg_send_mode`]).
    pub(crate) by_ref: bool,
    /// What the `INIT` of a call to the function writes into the call's
    /// flags: php's `quick_arg_flags`, which of the first twelve
    /// arguments it takes by value, for the `SEND_*_EX` oplines to test
    /// with no lookup ([`exec::CallInfo::sends_by_value`]); and whether
    /// its activation is plain, no `#[\Deprecated]` or `#[\NoDiscard]`
    /// to raise ([`exec::CallInfo::plain_entry`]).
    pub(crate) sent: exec::CallInfo,
}

impl FrameShape {
    /// `zend_vm_calc_used_stack` past the header for a call passing
    /// `count` arguments: the CVs and the temporaries, and the arguments
    /// past the declared parameters, which the activation moves past
    /// them (`zend_copy_extra_args`).
    pub(crate) fn room(self, count: u32) -> u32 {
        (self.slots.saturating_add(u32::from(self.observed)))
            .saturating_add(count.saturating_sub(self.parameters))
    }

    pub(crate) fn of(
        decl: &FunctionDecl,
        code: Option<&php_unit::OpArray>,
        observed: bool,
    ) -> FrameShape {
        let declared = decl.parameters.iter().filter(|p| !p.variadic).count();
        FrameShape {
            parameters: u32::try_from(declared).unwrap_or(u32::MAX),
            typed: decl.parameters.iter().any(|p| p.ty.is_some()),
            cvs: code.map_or(0, |code| code.num_cvs),
            slots: code.map_or(0, |code| code.num_cvs.saturating_add(code.num_tmps)),
            observed,
            entry: code.map_or(0, exec::entry_of),
            cache: code.map_or_else(exec::CacheAt::default, |code| {
                exec::CacheAt::of(code.runtime_cache)
            }),
            cache_size: code.map_or_else(exec::CacheSize::default, exec::CacheSize::of),
            by_ref: decl.parameters.iter().any(|p| p.by_ref),
            sent: exec::CallInfo::sent(|number| {
                sent_parameter(decl, number).is_some_and(|parameter| parameter.by_ref)
            })
            .with_plain_entry({
                let flags = crate::attributes::Flags::of(&decl.attributes);
                !flags.deprecated && !flags.no_discard
            }),
        }
    }
}

/// The parameter of `decl` the argument numbered `number` is sent to,
/// `zend_check_arg_send_type`'s `arg_info`: the declared one at its
/// place, or past the declared ones the variadic; none past them
/// without one.
pub(crate) fn sent_parameter(
    decl: &FunctionDecl,
    number: u32,
) -> Option<&php_unit::decl::Parameter> {
    let at = usize::try_from(number.saturating_sub(1)).unwrap_or(usize::MAX);
    let variadic = decl.parameters.last().filter(|last| last.variadic);
    decl.parameters
        .get(at)
        .filter(|parameter| !parameter.variadic)
        .or(variadic)
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

/// One class a request declared or the engine published: the unit that
/// holds its declaration and the declaration's index there, for a class
/// that uses traits the class composed with them at its link, which is
/// the table's row in its place (`zend_do_bind_traits`); and what
/// binding it settled ([`Bound`]), its placed properties among it.
/// Where it stands in `EG(class_table)` is the table's ([`Named`]), not
/// the row's.
pub(crate) struct Declared {
    unit: PublishedUnit,
    at: u32,
    composed: Option<Box<php_link::Composed>>,
    bound: Bound,
}

/// What binding a declared class settled once, for every lookup after
/// to read rather than walk ([`Request::bind_members`]).
#[derive(Default)]
pub(crate) struct Bound {
    /// The class's linked record: `ce->function_table` whole, its own
    /// methods and what it inherited, in php's order. None only while
    /// the class binds.
    record: Option<LinkedClass<ClassId>>,
    /// The function each of the class's own methods is, by its index
    /// among them, which a row of a record naming the class reads.
    functions: Box<[Option<Function>]>,
    /// The numbers the class's methods and hooks took as user
    /// functions, first to last, all in the class's own space: the
    /// request's where the class is its own, the arena's where it is
    /// published.
    members: Range<u32>,
    /// `ce->parent`, where its name named a class as the class bound;
    /// none for a class with no parent and for one bound before its
    /// parent was, whose parent is looked up by name.
    parent: Option<ClassId>,
    /// The class's placed properties, built as it bound
    /// ([`objects::placed_of`]): plain data, which a published class's
    /// row carries from its publication on.
    placed: Placed,
}

/// Where a class's declared properties are on its objects, built once
/// where the class is linked ([`objects::placed_of`]) and read as plain
/// data after.
#[derive(Debug, Clone, Default)]
pub(crate) struct Placed {
    /// The declared instance properties in slot order, each the row of
    /// the class that declared it: `ce->properties_info_table` over
    /// `ce->default_properties_table` ([`Request::placed_properties`]).
    slots: Box<[PropertyInfo]>,
    /// What each row of the class's linked record's `properties`,
    /// `ce->properties_info`, places its name at, by the row's position:
    /// what `zend_get_property_offset` reads off the
    /// `zend_property_info` its one find answers. Empty for a class with
    /// no record.
    rows: Box<[RowPlace]>,
    /// The slots `do_inherit_property` leaves `IS_UNDEF` in
    /// `default_properties_table` over the lineage, one for each property
    /// a class redeclares over its parent's: php's object keeps them
    /// though no property names one, and this engine places the property
    /// in one slot alone ([`objects::placed_of`]).
    holes: u32,
}

impl Placed {
    /// The declared instance properties in slot order.
    pub(crate) fn slots(&self) -> &[PropertyInfo] {
        &self.slots
    }

    /// [`Placed::holes`].
    pub(crate) fn holes(&self) -> u32 {
        self.holes
    }
}

/// What one row of `ce->properties_info` places its name at: the slot
/// its property takes on the class's objects, `property_info->offset`,
/// none for a `static` or a virtual one; and `ZEND_ACC_CHANGED`, whether
/// a class above it declares a private property of the name, which is
/// what makes a public property's placement read the scope; and what a
/// write to the slot is held to, its type's mask and flags read once
/// where the class's `properties_info` is built, as php's `prop_info`
/// carries them (a kernel placement reads them off the declaration it
/// placed, `DeclaredSlot`'s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowPlace {
    pub(crate) slot: Option<u32>,
    pub(crate) changed: bool,
    pub(crate) ty: crate::exec::SlotType,
}

/// `zend_property_info *`: a declared property named by the class that
/// declared it, its `ce`, and its place among that class's declared
/// properties, where its declaration is read
/// ([`Request::property_decl`]). Two words, so a class's slots hold
/// what php's `properties_info_table` holds, pointers into the declaring
/// class, and no copy of a declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PropertyInfo {
    /// `prop_info->ce`: the class whose declaration it is, the scope its
    /// visibility is measured against and, for a `static` one, the class
    /// whose slot holds it.
    pub(crate) declaring: ClassId,
    /// Its index among `declaring`'s declared properties.
    pub(crate) at: u32,
}

/// A table keyed by a number, an object's id or a type's: php's
/// `HashTable` over an integer key, whose hash is the key
/// (`ZEND_HANDLE_NUMERIC`), here spread by one multiply so the lookup's
/// tag bits are not all zero for small numbers. Nothing to seed, so
/// nothing a request pays `SipHash` for.
pub(crate) type WordMap<K, V> =
    std::collections::HashMap<K, V, std::hash::BuildHasherDefault<WordHasher>>;

/// [`WordMap`]'s hash: the key's word times the golden ratio's.
#[derive(Default)]
pub(crate) struct WordHasher(u64);

impl std::hash::Hasher for WordHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(self.0.rotate_left(8) ^ u64::from(byte));
        }
    }

    fn write_u32(&mut self, word: u32) {
        self.write_u64(u64::from(word));
    }

    fn write_u64(&mut self, word: u64) {
        self.0 = (self.0 ^ word).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

impl Declared {
    /// The class's linked record, once it bound.
    pub(crate) fn record(&self) -> Option<&LinkedClass<ClassId>> {
        self.bound.record.as_ref()
    }

    /// The declaration the table holds for the class.
    pub(crate) fn decl(&self) -> Option<&ClassDecl> {
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
    let (table, at) = match user.body {
        exec::Body::Function(at) => (&user.unit.functions, at),
        exec::Body::Dynamic(at) => (&user.unit.dynamic, at),
        exec::Body::Main | exec::Body::Method(_) | exec::Body::Hook(_) => return None,
    };
    Some(&table.get(usize::try_from(at).ok()?)?.0)
}

impl Request {
    /// How many observers the engine declared, the one load each of the
    /// observer's homes tests (design §4.3 seam 9).
    #[inline(always)] // every home's test
    pub(crate) fn observers(&self) -> u32 {
        self.observers
    }

    /// The tier's state the request was laid with, shared: the one way to
    /// it outside `api::request`, so nothing there borrows it mutably or
    /// replaces it ([`crate::tier::state`]).
    #[inline]
    pub(crate) fn tier_state(&self) -> &crate::tier::TierState {
        &self.tier
    }

    /// The loops the request is served in, which every serving it
    /// starts takes (#3362).
    #[inline]
    pub(crate) fn loops(&self) -> &crate::tier::Loops {
        &self.tier.loops
    }

    /// The request published a unit whose words count (#3362): the
    /// engine notes it, so every request starting after starts in the
    /// tier's loop, and where this one is in `Nop`'s beside the tier's,
    /// a hand-off is due and the interrupt raised for the check that
    /// takes it, as publishing raises the publishing request's interrupt
    /// (ADR-0006, amended on #3301).
    pub(crate) fn published_counting(&self) {
        self.shared.binding.published();
        if self.tier.published_counting() {
            self.limits().interrupt().raise();
        }
    }

    /// The hand-off from `Nop`'s loop to the tier's, where one is due
    /// (`crate::tier::interrupted_at`): the request's tier laid again
    /// with the tier's loops and the state they lay, for the rest of the
    /// request. Whether it was due. No tier lends its state in `Nop`'s
    /// loop, so the old state drops with no lend standing.
    pub(crate) fn hand_off(&mut self) -> bool {
        let Some(handed) = self.tier.handed(&self.shared) else {
            return false;
        };
        self.tier = handed;
        true
    }

    /// A request no host lent a sink, which keeps what it writes in one
    /// of its own that no host reads: the engine's startup, its end where
    /// the host left no sink ([`EngineBuilder::ends_into`](crate::engine::EngineBuilder::ends_into)), and a unit
    /// test's.
    pub(crate) fn new(shared: Arc<Shared>, description: RequestDescription) -> Box<Request> {
        Request::lent(shared, description, Box::new(response::Kept::default()))
    }

    /// A request the host lent `sink`, which every call reaches as the
    /// request makes it and which [`Request::release_sink`] hands back
    /// (design §4.3 seam 4).
    ///
    /// The request is boxed here, once, and held by its box wherever it
    /// is served: php's request state is `EG()`, a global, so none of it
    /// stands on the C stack its walks recurse down, and a `Request` held
    /// by value put its ten kilobytes, and a copy, under every frame of
    /// the request (#3209). A call of its own, so the value made in place
    /// before it is boxed stands in this frame and no caller's.
    #[allow(clippy::too_many_lines)] // one line per field of the request
    #[inline(never)]
    pub(crate) fn lent(
        shared: Arc<Shared>,
        description: RequestDescription,
        sink: Box<dyn OutputSink + Send>,
    ) -> Box<Request> {
        let cwd = description.cwd.clone();
        let exit_status = description.exit_status;
        let output = Output::activate(description.web.as_ref());
        let resources = Resources::default();
        // `init_executor`'s `zend_vm_stack_init`: the first page of the
        // VM stack, out of the heap the object store's table stands in.
        // The main chunk holds it, so nothing refuses it; a heap that
        // did would refuse the first push again, with php's fatal.
        let mut heap = Heap::new_in(Arc::clone(&shared.chunks));
        #[cfg(debug_assertions)]
        if shared.sense_leaks == crate::engine::LeakSensor::Traced {
            heap.trace_allocations();
        }
        let mut vm = Vm::default();
        let _laid = vm.stack.open(&mut heap);
        let tables = preload::Tables::of(shared.units.preloaded());
        let buckets = preload::Buckets::past(tables.classes.len() + tables.aliases.len());
        // `accel_activate`'s restart, where one is pending: a new epoch,
        // its arena and the unit cache's files empty, handed to this
        // request whatever requests of the old one still run.
        let arena = shared
            .arena
            .enter(|arena| shared.units.restart(&arena.files));
        let typed_ini = shared.typed_ini;
        let observers = shared.observers;
        let mut req = Box::new(Request {
            // Fields are made in the order written: the request stands
            // inside the engine before its deadlines are laid.
            _inside: crate::fork::Inside::enter(),
            limits: limits::LimitsState::new(&shared.timer),
            tier: crate::tier::TierState::laid(&shared),
            tier_setting: shared.binding.setting,
            units_left: crate::tier::UnitsLeft::default(),
            shared,
            description,
            inspect: inspect::Inspected::default(),
            shared_hold: memory::SharedHold::Untaken,
            heap,
            vm,
            annexes: crate::exec::Annexes::default(),
            map: crate::exec::RuntimeCaches::default(),
            observers,
            state: state::RequestState::default(),
            ini: ini::IniState::new(typed_ini),
            diagnostics: diagnostics::DiagnosticsState::default(),
            response: response::ResponseState::new(sink, output),
            ending: None,
            ropes: ropes::Ropes::default(),
            exit_status,
            resources,
            http_globals: variables::HttpGlobals::default(),
            post: post::Post::default(),
            variables: variables::VariablesState::default(),
            tables: vec![SymbolTable::global()],
            symtable_cache: Vec::new(),
            included_files: include::IncludedFiles::default(),
            constants: constants::Constants::default(),
            class_constants: class_constants::ClassConstants::default(),
            class_natives: Vec::new(),
            calls: calls::Calls::default(),
            properties: properties::Properties::default(),
            site_override: None,
            static_props: static_props::StaticProps::default(),
            class_data: class_data::ClassDataTables::default(),
            weakrefs: crate::weakrefs::Table::default(),
            lazy: lazy::Table::default(),
            include: include::Include::default(),
            stream_thrown: None,
            shutdown_functions: shutdown::ShutdownFunctions::default(),
            ticks: ticks::Ticks::default(),
            autoload: autoload::Autoload::default(),
            environment: environment::EnvironmentState::new(),
            cwd,
            fibers: crate::fiber::Fibers::default(),
            raised: None,
            stepping_past: None,
            bailout: false,
            observing: observing::Observing::default(),
            frame_ids: frame_reads::FrameIds::default(),
            progress: ending::Progress::default(),
            objects: objects::Objects::default(),
            returned: Value::null(),
            declared: tables.classes,
            own_classes: Vec::new(),
            arena,
            classes: classes::Classes::default(),
            links: linking::Links::default(),
            record: recording::Record::default(),
            keys_made: 0,
            compile_arena: php_compile::syntax::Arena::default(),
            spare_unit: None,
            compiling_host_string: false,
            no_extensions: false,
            script_encodings: None,
            buckets,
            aliases: tables.aliases,
            user_functions: PinnedRows::default(),
            named_callees: std::collections::HashMap::new(),
            unit_strings: unit_strings::UnitStrings::default(),
            compiler_stacks: memory::CompilerStacks::default(),
            functions: tables.functions,
            static_vars: static_vars::StaticVars::default(),
            preloading: None,
            accel_loaded: None,
            rooms: rooms::Rooms::default(),
            early_bound: None,
            #[cfg(debug_assertions)]
            sensing: sensing::Sensing::default(),
            hold: hold::Holding::default(),
            traces: traces::Scratch::default(),
            foreign: foreign::Table::default(),
        });
        // The word a delayed OOM raises, php's `EG(vm_interrupt)`, which
        // `zend_mm_delayed_verror` stores (#3260).
        let word = req.limits().interrupt().word();
        req.heap.wake_on_oom(word);
        req.activate();
        req
    }

    /// `php_request_startup` and `zend_activate` over a request just
    /// laid: the host's constant, each extension's request state, the
    /// timer, the memory limit, the collector, the error level and the
    /// stack limit.
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
        let enable_gc = self.ini_flag(b"zend.enable_gc");
        self.heap.gc_startup(enable_gc);
        self.update_error_reporting();
        self.start_stack_limit();
    }

    /// `zend_object_std_dtor` over an object being freed, which `Std`'s
    /// `free_obj` is ([`php_heap::Heap::object_std_dtor`]): its weak
    /// references told and its members freed, once.
    pub(crate) fn free_members(&mut self, object: PhpObject) {
        Heap::object_std_dtor(&mut Hooks(self), object.heap());
    }

    /// php's `EG(weakrefs)`, whose rows only `crate::weakrefs` reads,
    /// the table's methods being private to it.
    pub(crate) fn weakrefs(&self) -> &crate::weakrefs::Table {
        &self.weakrefs
    }

    /// [`Request::weakrefs`], to register a holder or take one off.
    pub(crate) fn weakrefs_mut(&mut self) -> &mut crate::weakrefs::Table {
        &mut self.weakrefs
    }

    /// `CG(rtd_key_counter)` over the compiles made for this request, its
    /// own ([`crate::engine::Shared::compile_for`]) and the files the unit
    /// cache compiled for it ([`crate::engine::Shared::compile_file`]):
    /// where the next compile counts its runtime definition keys on from.
    pub(crate) fn keys_made(&self) -> u32 {
        self.keys_made
    }

    /// The `made` runtime definition keys of a compile just made for this
    /// request, counted on, saturating at `u32::MAX`.
    pub(crate) fn count_keys_made(&mut self, made: u32) {
        self.keys_made = self.keys_made.saturating_add(made);
    }
}

impl<'r> Diagnostics<&'r mut Request> {
    /// The slot for a throwable PHP code served by a stream's kind, a
    /// filter or a user-space wrapper raised inside an operation whose
    /// answer has no room for it: php's `EG(exception)` standing until
    /// the native that asked for the operation returns, which raises it,
    /// and before which no more such code is served.
    pub fn stream_thrown(self) -> &'r mut Option<Thrown> {
        &mut self.0.stream_thrown
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

impl Hooks<'_> {
    /// A step a release serves, a class's `dtor_obj` or `free_obj` or a
    /// resource type's destructor, and what PHP code it served threw,
    /// raised where the release stands as `zend_throw_exception_internal`
    /// raises it ([`Request::throw_internal`]). A stream the step closed
    /// whose user-space wrapper or filter threw parks the throwable for
    /// the native that asked to raise once it returns
    /// ([`Diagnostics::stream_thrown`]); a release is no native, so the step
    /// raises it here, where nothing was parked before the step began.
    /// What the step answered as its `Err` is raised after that, so it
    /// chains over the parked one, as php's `EG(exception)` takes both
    /// in the order they were thrown.
    ///
    /// ```php
    /// class W {
    ///   public $context;
    ///   function stream_open($p, $m, $o, &$op) { return true; }
    ///   function stream_read($n) { return ''; }
    ///   function stream_eof() { return true; }
    ///   function stream_stat() { return []; }
    ///   function url_stat($p, $f) { return ["mode" => 0100644]; }
    ///   function stream_close() { echo "close\n"; throw new Exception("close"); }
    /// }
    /// stream_wrapper_register('ww', 'W');
    /// $f = new SplFileObject('ww://x');
    /// try { $f = null; echo "after\n"; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
    /// echo "end\n";
    /// ```
    /// ```expect
    /// close
    /// caught close
    /// end
    /// ```
    ///
    /// With no frame standing, at the request's end, it is handed to the
    /// handler `set_exception_handler()` set:
    ///
    /// ```php
    /// class W {
    ///   public $context;
    ///   function stream_open($p, $m, $o, &$op) { return true; }
    ///   function stream_read($n) { return ''; }
    ///   function stream_eof() { return true; }
    ///   function stream_stat() { return []; }
    ///   function url_stat($p, $f) { return ["mode" => 0100644]; }
    ///   function stream_close() { echo "close\n"; throw new Exception("close"); }
    /// }
    /// stream_wrapper_register('ww', 'W');
    /// set_exception_handler(function ($e) { echo "handler ", $e->getMessage(), "\n"; });
    /// $f = new SplFileObject('ww://x');
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    /// close
    /// handler close
    /// ```
    fn released(&mut self, step: impl FnOnce(&mut Request) -> Result<(), Thrown>) {
        let parked = self.0.stream_thrown.is_some();
        let answered = step(self.0);
        if !parked && let Some(thrown) = self.0.stream_thrown.take() {
            self.0.throw_internal(&thrown);
        }
        if let Err(thrown) = answered {
            self.0.throw_internal(&thrown);
        }
    }
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

    /// `dtor_obj`, through the table of the object's class (design §5.4),
    /// as a release serves it ([`Hooks::released`]).
    fn destruct(&mut self, obj: Obj) {
        let table = self.0.object_handlers(obj);
        self.released(|req| {
            table.dtor_obj().serve(req, PhpObject::of(obj));
            Ok(())
        });
    }

    /// `free_obj`, through the table, as a release serves it
    /// ([`Hooks::released`]): the payload's release is the class's, and
    /// the drop after it is the heap's.
    fn free_native(&mut self, obj: Obj) {
        let table = self.0.object_handlers(obj);
        self.released(|req| {
            table.free_obj(req, PhpObject::of(obj));
            Ok(())
        });
        self.0.free_beside(PhpObject::of(obj));
    }

    /// The destructor phase served in the collector's own fiber while
    /// a fiber runs ([`crate::fiber::gc_destructors_in_fiber`]).
    fn gc_destructors_in_fiber(&mut self, from: u32, end: u32) -> bool {
        crate::fiber::gc_destructors_in_fiber(self.0, from, end)
    }

    /// php's `dtor_obj != zend_objects_destroy_object || ce->destructor`
    /// ([`Request::has_destructor`]). A generator the collector finds is
    /// destroyed before it is freed, its `finally` blocks served, and it
    /// is freed by the release of what held it, not counted among what
    /// the collection freed.
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
    ///
    /// The iterator a `RecursiveIteratorIterator` keeps of its inner
    /// iterator is php's `zend_object_iterator`, whose table's `dtor_obj`
    /// is `iter_wrapper_dtor` and not the standard one, so the collector
    /// serves it first and leaves what it reaches out of the count: the
    /// cycle through the outer iterator is one value freed, and the rest
    /// is freed by the release of what held it.
    ///
    /// ```php
    /// class R extends RecursiveIteratorIterator { public $self; }
    /// function t() { $it = new R(new RecursiveArrayIterator([1])); $it->self = $it; unset($it); var_dump(gc_collect_cycles()); }
    /// t(); t();
    /// ```
    /// ```expect
    /// int(1)
    /// int(1)
    /// ```
    fn has_destructor(&mut self, obj: Obj) -> bool {
        self.0.has_destructor(PhpObject::of(obj))
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
        for at in (0..req.vm.depth()).rev() {
            let Some(state) = req.frame_on(&req.vm, at) else {
                continue;
            };
            let Some(code) = state.code() else {
                continue;
            };
            let opline = state.opline();
            let frame = FrameRef(u32::try_from(at).unwrap_or(u32::MAX));
            for range in &code.live_ranges {
                if range.start.0 > opline {
                    break;
                }
                if range.end.0 <= opline {
                    continue;
                }
                if matches!(
                    range.kind,
                    php_unit::LiveKind::TmpVar | php_unit::LiveKind::Loop
                ) {
                    let slot = range.var;
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
    /// collector's walk as zvals, and the table the handler returns.
    fn gc_children(&mut self, obj: Obj, visit: &mut dyn FnMut(Zval)) -> php_heap::GcTable {
        let handlers = self.0.object_handlers(obj);
        let table = handlers.get_gc(self.0, PhpObject::of(obj), &mut |value| {
            visit(value.to_zval());
        });
        // `zend_lazy_object_get_gc`: the initializer a lazy object waits
        // for, or the instance a proxy forwards to.
        self.0.lazy_gc(obj, &mut |value| visit(value.to_zval()));
        match table {
            GcTable::Properties => php_heap::GcTable::Properties,
            GcTable::Owned(table) => php_heap::GcTable::Arr(table.heap()),
            GcTable::Rows(rows) => {
                php_heap::GcTable::Rows(rows.into_iter().map(Value::to_zval).collect())
            }
        }
    }

    /// `GC buffer overflow (GC disabled)`, php's warning with its own
    /// newline, raised where the request stands.
    fn gc_overflow(&mut self) {
        crate::api::raising::warning(self.0, "GC buffer overflow (GC disabled)\n");
    }

    /// `list_dtor_ex`: the type's destructor as registered, handed the
    /// resource with the type it had, as php hands it a copy of the
    /// struct; a type with no destructor closes silently. What it throws,
    /// answered as its `Err` or parked by a stream it closed, is raised
    /// as a release raises it ([`Hooks::released`]).
    ///
    /// A stream an assignment releases, whose user-space wrapper's
    /// `stream_close` throws, is the release's to raise (#2536):
    ///
    /// ```php
    /// class W {
    ///   public $context;
    ///   function stream_open($p, $m, $o, &$op) { return true; }
    ///   function stream_read($n) { return ''; }
    ///   function stream_eof() { return true; }
    ///   function stream_close() { echo "close\n"; throw new Exception("close"); }
    /// }
    /// stream_wrapper_register('ww', 'W');
    /// $h = fopen('ww://y', 'r');
    /// try { $h = null; echo "after\n"; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
    /// function f() { $h = fopen('ww://y', 'r'); return 1; }
    /// try { f(); echo "after\n"; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
    /// try { $a = [fopen('ww://y', 'r')]; array_pop($a); echo "after\n"; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
    /// echo "end\n";
    /// ```
    /// ```expect
    /// close
    /// caught close
    /// close
    /// caught close
    /// close
    /// caught close
    /// end
    /// ```
    ///
    /// A user filter the stream's free flushes throws into the same
    /// release:
    ///
    /// ```php
    /// class F extends php_user_filter {
    ///   function filter($in, $out, &$consumed, $closing): int {
    ///     while ($b = stream_bucket_make_writeable($in)) { $consumed += $b->datalen; stream_bucket_append($out, $b); }
    ///     if ($closing) { echo "closing\n"; throw new Exception("filter"); }
    ///     return PSFS_PASS_ON;
    ///   }
    /// }
    /// stream_filter_register('f', 'F');
    /// $h = fopen('php://memory', 'w+');
    /// stream_filter_append($h, 'f', STREAM_FILTER_WRITE);
    /// fwrite($h, "x");
    /// try { $h = null; echo "after\n"; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
    /// echo "end\n";
    /// ```
    /// ```expect
    /// closing
    /// caught filter
    /// end
    /// ```
    fn free_resource(&mut self, res: Res, kind: ResourceType) {
        if let Some(dtor) = self.0.resources.dtor(kind) {
            self.released(|req| dtor(req, Resource::of(res)));
        }
        // `zend_close_rsrc_list`'s `zend_catch`: what a destructor bailed
        // out of is caught behind it, and the walk goes on.
        if self.0.phase() == Phase::ResourceShutdown {
            self.0.catch_bailout();
        }
    }

    fn out_of_memory(&mut self, bytes: usize) -> Fatal {
        Fatal::out_of_memory(bytes)
    }

    /// `zend_free_source_list`: the row the cell's word names, dropped
    /// with the cell. The heap has already read the word, so nothing
    /// here reads the reference again.
    fn free_reference(&mut self, r: Ref) {
        self.0.free_source_list(r.sources());
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
    /// [`Request::push_code_frame`] of a program in a test, whose request
    /// no memory limit holds, so the push is never refused.
    pub(crate) fn pushed(&mut self, unit: PublishedUnit) -> FrameRef {
        let pushed = self.push_code_frame(unit, CodeFrom::Top(None));
        assert_eq!(pushed.as_ref().err(), None, "a test's frame is pushed");
        pushed.unwrap_or(FrameRef(0))
    }
}

#[cfg(test)]
mod tests {
    use super::Request;
    use crate::engine::Engine;

    /// A tier state laid for the test, so a lend has a state to lend.
    struct Laying;

    impl crate::tier::Tier for Laying {
        type State = u32;

        fn safepoint(
            _: &mut Request,
            _: php_heap::FrameRef,
            _: crate::tier::Site,
            _: crate::tier::Held<'_>,
        ) -> crate::tier::Flow {
            crate::tier::Flow::Continue
        }

        fn lay(_: &crate::tier::Started<'_>) -> Option<u32> {
            Some(7)
        }
    }

    /// The rule the lend of a request's tier state stands on (#3437,
    /// `php_jit`'s `enter::lent`): `api::request`, the one place the
    /// types leave able to, replacing the state while a tier lends it
    /// fails the lend's debug assertion as the old state drops.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "while a tier lent it")]
    fn a_tier_state_replaced_while_lent_fails_the_lends_assertion() {
        let engine = Engine::cli_builder().tier::<Laying>().build();
        let mut req = Request::new(
            std::sync::Arc::clone(&engine.shared),
            crate::engine::RequestDescription::default(),
        );
        assert_eq!(
            crate::tier::state::<u32>(&req),
            Some(&7),
            "the state is laid"
        );
        let lending = crate::tier::lending(&req);
        req.tier = crate::tier::TierState::laid(&req.shared);
        drop(lending);
    }

    /// The same state, replaced once no lend stands: no assertion fails.
    #[test]
    fn a_tier_state_replaced_with_no_lend_standing_is_dropped_quietly() {
        let engine = Engine::cli_builder().tier::<Laying>().build();
        let mut req = Request::new(
            std::sync::Arc::clone(&engine.shared),
            crate::engine::RequestDescription::default(),
        );
        drop(crate::tier::lending(&req));
        req.tier = crate::tier::TierState::laid(&req.shared);
        assert_eq!(
            crate::tier::state::<u32>(&req),
            Some(&7),
            "the state is laid again"
        );
    }

    /// A class declaring `__destruct`, as an extension declares one:
    /// what [`Request::destruct`] looks for on the class of the object
    /// it is handed. Its entry is never served, since the branch under
    /// test refuses the call before it.
    fn destructible() -> php_unit::decl::ClassDecl {
        use php_unit::decl::{
            ClassDecl, ClassFlags, ClassKind, EnumBacking, MethodDecl, Name, Visibility,
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
                function: php_unit::decl::native_function(b"__destruct"),
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
            attribute_validator: None,
        }
    }

    /// The entry of the `__destruct` above: the branch under test
    /// refuses the call, so a raise here is the test failing.
    fn never_served(
        req: &mut Request,
    ) -> Result<crate::api::values::Owner<crate::api::values::Value>, super::Thrown> {
        Err(crate::api::raising::error(req, "the destructor was served"))
    }

    /// The same entry, serving: what a native class's `__destruct` is,
    /// a Rust body on the request that served it, which gives up the
    /// string it made once it is written.
    fn writes_d(
        req: &mut Request,
    ) -> Result<crate::api::values::Owner<crate::api::values::Value>, super::Thrown> {
        let text = req.string(b"d\n")?;
        req.response_mut().write(text.as_bytes());
        req.release(crate::api::values::Value::string(text));
        let null = crate::api::values::Value::null();
        Ok(crate::api::values::Owner::take(req, null))
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
        let method = decl.methods.first().cloned().expect("the first method");
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

        served.assert_clean("d\nafter\n");
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
        let method = decl.methods.first().cloned().expect("the first method");
        let destructible = crate::api::abi::ExtensionBuilder::default()
            .classes(vec![(
                decl,
                vec![(method, never_served as crate::api::abi::Entry)],
            )])
            .build("destructible");
        let engine = Engine::cli_builder().extensions(&[destructible]).build();
        let mut request = crate::test::request_on(&engine);
        let object = request
            .new_object_of("Destructible")
            .expect("a Destructible");
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
}

#[cfg(test)]
mod pinned_rows {
    use super::PinnedRows;

    /// Every row stands where it was pushed while the rows grow past the
    /// first segment and three later ones, and reads back by its place.
    #[test]
    fn a_row_stands_where_it_was_pushed() {
        let mut rows = PinnedRows::default();
        let mut places = Vec::new();
        for at in 0..2000usize {
            rows.push(at);
            places.push(std::ptr::from_ref(rows.get(at).unwrap_or(&usize::MAX)).addr());
        }
        assert_eq!(rows.len(), 2000);
        for (at, place) in places.iter().enumerate() {
            let row = rows.get(at);
            assert_eq!(row, Some(&at));
            assert_eq!(row.map(|row| std::ptr::from_ref(row).addr()), Some(*place));
        }
        assert_eq!(rows.get(2000), None);
        assert!(rows.iter().copied().eq(0..2000));
        assert!(rows.into_iter().eq(0..2000));
    }
}
