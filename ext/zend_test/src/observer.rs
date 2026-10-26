//! php's test observer, `ext/zend_test/observer.c` (design §4.3 seam 9):
//! the call observer that writes each observed frame's begin and end as
//! a tag, nested two spaces a level, and the fiber observers that write
//! each fiber's creation, switches and destruction, each behind one of
//! the `zend_test.observer.*` switches its tests set.
//!
//! The switches are `PHP_INI_SYSTEM`, so they are read once, as the
//! engine starts, into the engine state [`Switches`], php's `ZT_G`
//! words as `MINIT` leaves them; `zend_test.observer.enabled` is what
//! php's `MINIT` registers the observers under, and here they are
//! declared always and answer nothing while it is off, which nothing a
//! program prints tells apart. `zend_test.observer.observe_function_names`
//! is the one entry a request changes, and its value is read where the
//! observer is asked of a function, as php reads the table its handler
//! keeps.
//!
//! `zend_test.observer.execute_internal` sets php's `zend_execute_internal`
//! to a function that writes `<!-- internal enter f() -->` and serves
//! the native. The engine's equivalent of that pointer for a tool is the
//! observer of natives (#1770), so the switch is a second observer,
//! [`execute_internal_init`], declared after the first: its begin
//! handler is called after [`fcall_init`]'s, where php's `ZEND_DO_FCALL`
//! calls `zend_execute_internal` after `ZEND_OBSERVER_FCALL_BEGIN`, and
//! whether or not the observer is enabled, as php sets the pointer.
//!
//! `zend_test.observer.show_opcode` writes the opcode a frame stands
//! at, read through [`FrameView::opcode`], where the init, the begin and
//! the end are written; `show_opcode_in_user_handler` names the opcodes
//! the extension observes (design §7.1, #1824), declared as the engine
//! starts, each written as it is told of one before its handler runs.

use std::fmt::Write as _;

use php_ext::abi::{
    Beneath, ExtensionBuilder, FiberStatus, IniStage, Observed, ObservedFiber, ObserverHandlers,
    Opcodes, Refusal, Start, chain,
};
use php_ext::call::{Function, lookup_function};
use php_ext::coerce::type_name;
use php_ext::decl::IniDecl;
use php_ext::raising::Thrown;
use php_ext::request::{ChainFrame, FrameView, Ini, Raised, Request};
use php_ext::values::{ClassId, Value};
use php_ext_standard::php_var_export_ex;

use crate::Entry;
use crate::Modifiable::{All, System};
use crate::throwables::Throwables;

/// `zend_test.observer.enabled`.
const ENABLED: &[u8] = b"zend_test.observer.enabled";

/// `zend_test.observer.show_opcode_in_user_handler`: the opcodes
/// observed, by php's names, separated by spaces or commas.
const SHOW_OPCODE_IN_USER_HANDLER: &[u8] = b"zend_test.observer.show_opcode_in_user_handler";

/// `zend_test.observer.observe_function_names`, `PHP_INI_ALL`: the
/// functions observed by name, comma-separated.
const OBSERVE_FUNCTION_NAMES: &[u8] = b"zend_test.observer.observe_function_names";

/// `observer.c`'s `PHP_INI_BEGIN` table, in its order.
pub(crate) const ENTRIES: [Entry; 18] = [
    Entry::boolean(ENABLED, b"0", System),
    Entry::boolean(b"zend_test.observer.show_output", b"1", System),
    Entry::boolean(b"zend_test.observer.observe_all", b"0", System),
    Entry::boolean(b"zend_test.observer.observe_includes", b"0", System),
    Entry::boolean(b"zend_test.observer.observe_functions", b"0", System),
    Entry::boolean(b"zend_test.observer.observe_declaring", b"0", System),
    Entry::plain(OBSERVE_FUNCTION_NAMES, b"", All),
    Entry::boolean(b"zend_test.observer.show_return_type", b"0", System),
    Entry::boolean(b"zend_test.observer.show_return_value", b"0", System),
    Entry::boolean(b"zend_test.observer.show_init_backtrace", b"0", System),
    Entry::boolean(b"zend_test.observer.show_opcode", b"0", System),
    Entry::plain(SHOW_OPCODE_IN_USER_HANDLER, b"", System),
    Entry::boolean(
        b"zend_test.observer.set_vm_interrupt_on_begin",
        b"0",
        System,
    ),
    Entry::boolean(b"zend_test.observer.fiber_init", b"0", System),
    Entry::boolean(b"zend_test.observer.fiber_switch", b"0", System),
    Entry::boolean(b"zend_test.observer.fiber_destroy", b"0", System),
    Entry::boolean(b"zend_test.observer.execute_internal", b"0", System),
    Entry::boolean(b"zend_test.observer.reserve_op_array_handle", b"0", System),
];

/// The observer as `zend_test_observer_init` registers it at `MINIT`:
/// the ini table, the switches read once as the engine starts, the
/// request state `RINIT` zeroes, the call observer and the one standing
/// for `zend_execute_internal`, the opcodes observed, and the fiber
/// observers. Every switch is
/// `PHP_INI_SYSTEM` but `observe_function_names`, which a request may
/// set only while the observer is enabled.
///
/// ```php
/// var_dump(ini_get('zend_test.observer.fiber_switch'), ini_get('zend_test.observer.observe_function_names'));
/// var_dump(ini_set('zend_test.observer.show_output', '0'), ini_set('zend_test.observer.observe_function_names', 'f'));
/// ```
/// ```expect
/// string(1) "0"
/// string(0) ""
/// bool(false)
/// bool(false)
/// ```
pub(crate) fn declared(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder
        .ini(ini())
        .on_modify(OBSERVE_FUNCTION_NAMES, on_update_function_names)
        .engine_state(Started::start)
        .observer_when(fcall_init, |start| started_on(start, ENABLED))
        .observer_when(execute_internal_init, |start| {
            started_on(start, b"zend_test.observer.execute_internal")
        })
        .opcodes(observed_opcodes, show_opcode_in_user_handler)
        .fiber_init(fiber_init)
        .fiber_switch(fiber_switch)
        .fiber_destroy(fiber_destroy)
        .function_declared_observer(declared_function)
        .class_linked_observer(declared_class)
}

/// The table as the extension declares it.
fn ini() -> Vec<IniDecl> {
    ENTRIES.iter().map(Entry::decl).collect()
}

/// The engine state: the switches and the classes the natives raise
/// (`crate::throwables`), both as the engine started with them.
#[derive(Clone, Copy)]
pub(crate) struct Started {
    switches: Switches,
    pub(crate) throwables: Throwables,
}

impl Started {
    /// The engine start: the switches read, the classes resolved.
    fn start(start: &Start) -> Result<Started, Refusal> {
        Ok(Started {
            switches: Switches::start(start)?,
            throwables: Throwables::start(start)?,
        })
    }
}

/// The `PHP_INI_SYSTEM` switches as the engine started with them,
/// `ZT_G(observer_*)` once `MINIT` has registered the table.
#[derive(Debug, Clone, Copy, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "observer.c's switches, one `int` each in `zend_zend_test_globals`"
)]
struct Switches {
    enabled: bool,
    show_output: bool,
    observe_all: bool,
    observe_includes: bool,
    observe_functions: bool,
    observe_declaring: bool,
    show_return_type: bool,
    show_return_value: bool,
    show_init_backtrace: bool,
    show_opcode: bool,
    set_vm_interrupt_on_begin: bool,
    fiber_init: bool,
    fiber_switch: bool,
    fiber_destroy: bool,
    execute_internal: bool,
}

impl Switches {
    /// The engine start: each switch read from the startup ini, its
    /// default where the host set none.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the engine start's shape, which a start that refuses needs"
    )]
    fn start(start: &Start) -> Result<Switches, Refusal> {
        let on = |name: &[u8]| started_on(start, name);
        let switch = |suffix: &str| on(format!("zend_test.observer.{suffix}").as_bytes());
        Ok(Switches {
            enabled: on(ENABLED),
            show_output: switch("show_output"),
            observe_all: switch("observe_all"),
            observe_includes: switch("observe_includes"),
            observe_functions: switch("observe_functions"),
            observe_declaring: switch("observe_declaring"),
            show_return_type: switch("show_return_type"),
            show_return_value: switch("show_return_value"),
            show_init_backtrace: switch("show_init_backtrace"),
            show_opcode: switch("show_opcode"),
            set_vm_interrupt_on_begin: switch("set_vm_interrupt_on_begin"),
            fiber_init: switch("fiber_init"),
            fiber_switch: switch("fiber_switch"),
            fiber_destroy: switch("fiber_destroy"),
            execute_internal: switch("execute_internal"),
        })
    }

    /// The switches of the engine `req` is served by.
    fn of(req: &Request) -> Switches {
        req.engine::<Started>()
            .map_or_else(Switches::default, |state| state.switches)
    }
}

/// Whether the switch `name` is on in the startup ini: what `MINIT`
/// reads to register the call observer, `zend_observer_fcall_register`
/// under `ZT_G(observer_enabled)`, and its `zend_execute_internal` under
/// `ZT_G(observer_execute_internal)`, which the engine serves as an
/// observer too.
fn started_on(start: &Start, name: &[u8]) -> bool {
    Ini::parse_bool(startup_value(start, name))
}

/// An entry's value in the startup ini, its default where the host set
/// none.
fn startup_value<'a>(start: &'a Start, name: &[u8]) -> &'a [u8] {
    let set = start
        .ini()
        .entries
        .iter()
        .rev()
        .find(|(entry, _)| entry.as_slice() == name)
        .map(|(_, value)| value.as_deref().unwrap_or_default());
    let default = ENTRIES
        .iter()
        .find(|entry| entry.name == name)
        .map_or(&b""[..], |entry| entry.default);
    set.unwrap_or(default)
}

/// `observer_set_user_opcode_handler` as `zend_test_observer_init`
/// calls it at `MINIT`: under `enabled`, each opcode
/// `show_opcode_in_user_handler` names, the names separated by spaces or
/// commas, found as php's `zend_get_opcode_id` finds one. A name no
/// opcode's opens with is passed over; php warns `Invalid opcode name`
/// there, and an engine start has no warning to raise that does not
/// refuse the start.
///
/// Under the compiled tier's force lane (`jit-design.md` §4.3,
/// *Observed opcodes*), the two silence opcodes are told from inside a
/// compiled loop as the interpreter tells them, and any other refuses
/// the tier with php's startup warning, as `zend_jit_check_support`
/// refuses it; the child is served with the switches on, which only
/// startup sets, and php ignores the engine's `jit.mode`:
///
/// ```php
/// $dir = sys_get_temp_dir() . '/opcodes' . getmypid(); @mkdir($dir);
/// file_put_contents("$dir/loop.php", '<?php
/// function f($x) { return $x + 1; }
/// $s = 0;
/// for ($i = 0; $i < 3; $i++) { $s += @f($i); }
/// echo $s, "\n";');
/// foreach (['ZEND_BEGIN_SILENCE ZEND_END_SILENCE', 'ZEND_ECHO'] as $opcodes) {
///     $p = proc_open([PHP_BINARY, '-n', '-d', 'display_errors=1', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=tracing',
///         '-d', 'opcache.jit_buffer_size=64M', '-d', 'jit.mode=force', '-d', 'zend_test.observer.enabled=1',
///         '-d', "zend_test.observer.show_opcode_in_user_handler=$opcodes", "$dir/loop.php"], [1 => ['pipe', 'w']], $pipes);
///     echo str_replace(realpath($dir), 'D', stream_get_contents($pipes[1])), "--\n"; proc_close($p);
/// }
/// exec('rm -rf ' . escapeshellarg($dir));
/// ```
/// ```expect process regime=jit-force
/// <!-- init 'D/loop.php' -->
/// <!-- opcode: 'ZEND_BEGIN_SILENCE' in user handler -->
/// <!-- init f() -->
/// <!-- opcode: 'ZEND_END_SILENCE' in user handler -->
/// <!-- opcode: 'ZEND_BEGIN_SILENCE' in user handler -->
/// <!-- opcode: 'ZEND_END_SILENCE' in user handler -->
/// <!-- opcode: 'ZEND_BEGIN_SILENCE' in user handler -->
/// <!-- opcode: 'ZEND_END_SILENCE' in user handler -->
/// 6
/// --
///
/// Warning: JIT is incompatible with third party extensions that setup user opcode handlers. JIT disabled. in Unknown on line 0
/// <!-- init 'D/loop.php' -->
/// <!-- init f() -->
/// <!-- opcode: 'ZEND_ECHO' in user handler -->
/// 6<!-- opcode: 'ZEND_ECHO' in user handler -->
///
/// --
/// ```
fn observed_opcodes(start: &Start) -> Opcodes {
    let mut opcodes = Opcodes::default();
    if !Ini::parse_bool(startup_value(start, ENABLED)) {
        return opcodes;
    }
    let names = startup_value(start, SHOW_OPCODE_IN_USER_HANDLER);
    for name in names.split(|&b| b == b' ' || b == b',') {
        if !name.is_empty() {
            opcodes.insert(name);
        }
    }
    opcodes
}

/// `observer_show_opcode_in_user_handler`: under `show_output`,
/// `<!-- opcode: 'ZEND_ECHO' in user handler -->` at the depth the
/// frames stand, then the opcode's handler, which always runs. php's
/// is set over whatever handler stood and saves none
/// (`Z/ext/zend_test/observer.c:28-35`), so the ones beneath are not
/// served.
fn show_opcode_in_user_handler(observed: &mut Observed<'_>, beneath: Beneath<'_, chain::Opcode>) {
    if !Switches::of(observed).show_output {
        return;
    }
    let mut line = indent(observed.state::<crate::State>().observer.depth);
    line.extend_from_slice(b"<!-- opcode: '");
    line.extend_from_slice(beneath.opcode().as_bytes());
    line.extend_from_slice(b"' in user handler -->\n");
    observed.response_mut().write(&line);
}

/// `observer_show_opcode`: under `show_opcode`, the opcode a user
/// frame stands at, `<!-- opcode: 'ZEND_ECHO' -->`, at the depth the
/// frames stand; nothing for a native's call.
fn show_opcode(observed: &mut Observed<'_>) {
    if !Switches::of(observed).show_opcode {
        return;
    }
    let Some(opcode) = observed.frame().map(|frame| frame.opcode()) else {
        return;
    };
    let mut line = indent(observed.state::<crate::State>().observer.depth);
    line.extend_from_slice(b"<!-- opcode: '");
    line.extend_from_slice(opcode.as_bytes());
    line.extend_from_slice(b"' -->\n");
    observed.response_mut().write(&line);
}

/// The observer's request state: `ZT_G(observer_nesting_depth)`, which
/// `RINIT` zeroes.
#[derive(Debug, Default)]
pub(crate) struct Observing {
    depth: usize,
}

/// The names `observe_function_names` holds, split at each comma as the
/// handler splits them, none where it is empty.
fn names_in(value: Option<&[u8]>) -> impl Iterator<Item = &[u8]> {
    value
        .filter(|names| !names.is_empty())
        .into_iter()
        .flat_map(|names| names.split(|&b| b == b','))
}

/// `ZEND_INI_MH(zend_test_observer_OnUpdateCommaList)`: refused while
/// the observer is not enabled, as `ZT_G(observer_enabled)` reads
/// whenever the handler runs. The table php keeps is the entry's value
/// here, which [`fcall_init`] reads. While a request runs, each function
/// the old value names has the observer's handlers taken out of its slot
/// and each the new value names has them added, as
/// `zend_observer_remove_begin_handler` and `zend_observer_add_begin_handler`
/// do; a function not called yet has no slot, and is answered by
/// [`fcall_init`] on its first call.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the `on_modify` slot's shape, which a handler that throws needs"
)]
fn on_update_function_names(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    // Read from the entry and not from [`Switches`]: at the engine's
    // start the handler runs before the engine state is built.
    if !req.ini().get(ENABLED).is_some_and(Ini::parse_bool) {
        return Ok(false);
    }
    if req.ini().stage() != IniStage::Runtime {
        return Ok(true);
    }
    // `zend_hash_find_ptr(EG(function_table), funcname)`: the table's
    // keys are lowered, so a name written with a capital finds nothing.
    let named = |req: &Request, names: Option<&[u8]>| -> Vec<Function> {
        names_in(names)
            .filter(|name| !name.iter().any(u8::is_ascii_uppercase))
            .filter_map(|name| lookup_function(req, name))
            .collect()
    };
    for function in named(req, req.ini().get(OBSERVE_FUNCTION_NAMES)) {
        let _ = req.code_mut().remove_begin_handler(function, begin);
        let _ = req.code_mut().remove_end_handler(function, end);
    }
    for function in named(req, value) {
        req.code_mut().add_begin_handler(function, begin);
        req.code_mut().add_end_handler(function, end);
    }
    Ok(true)
}

/// `declared_function_observer`: `<!-- declared function 'f' -->` under
/// `observe_declaring`, by the lowered name it is declared under.
fn declared_function(req: &mut Request, _function: Function, name: &[u8]) {
    show_declared(req, "function", name);
}

/// `declared_class_observer`: `<!-- declared class 'c' -->` under
/// `observe_declaring`.
fn declared_class(req: &mut Request, _class: ClassId, name: &[u8]) {
    show_declared(req, "class", name);
}

fn show_declared(req: &mut Request, what: &str, name: &[u8]) {
    let switches = Switches::of(req);
    if !(switches.enabled && switches.observe_declaring) {
        return;
    }
    let mut line = indent(req.state::<crate::State>().observer.depth);
    line.extend_from_slice(format!("<!-- declared {what} '").as_bytes());
    line.extend_from_slice(name);
    line.extend_from_slice(b"' -->\n");
    req.response_mut().write(&line);
}

/// What an observed frame serves: a function, a method or a native,
/// by its name and the name of its scope, or an included, evaluated or
/// main file's op array, by its file.
enum Serves {
    Function {
        scope: Option<String>,
        name: Vec<u8>,
    },
    File(Vec<u8>),
}

impl Serves {
    /// The frame an observer is handed, the native's call it is, or a
    /// frame a fatal left, read by its function or its file.
    fn of(observed: &Observed<'_>) -> Serves {
        match (observed.frame(), observed.function()) {
            (Some(frame), _) => Serves::frame(observed, &frame),
            (None, Some(function)) => {
                Serves::function_of(observed, function, observed.function_scope())
            }
            (None, None) => match observed.filename() {
                [] => Serves::File(NO_ACTIVE_FILE.to_vec()),
                file => Serves::File(file.to_vec()),
            },
        }
    }

    /// A frame the loop serves.
    fn frame(req: &Request, frame: &FrameView<'_>) -> Serves {
        match frame.function() {
            Some(function) => Serves::function_of(req, function, frame.function_scope()),
            None => Serves::File(frame.filename().to_vec()),
        }
    }

    /// A frame of php's `prev_execute_data` chain: one that names no
    /// function by its file, and a fiber's bottom frame, which names no
    /// file either, as php writes a NULL `op_array.filename`.
    fn chain(req: &Request, frame: &ChainFrame<'_>) -> Serves {
        match frame.function() {
            Some(function) => Serves::function_of(req, function, frame.function_scope()),
            None => Serves::File(frame.filename().unwrap_or(NO_ACTIVE_FILE).to_vec()),
        }
    }

    fn function_of(req: &Request, function: Function, scope: Option<ClassId>) -> Serves {
        Serves::Function {
            scope: scope
                .map(|class| String::from_utf8_lossy(req.symbols().class_name(class)).into_owned()),
            name: req
                .symbols()
                .function_decl(function)
                .map(|decl| decl.name.0.to_vec())
                .unwrap_or_default(),
        }
    }

    /// `%s::%s` or `%s`, as the tags name a function.
    fn function(scope: Option<&str>, name: &[u8]) -> Vec<u8> {
        let mut named = Vec::new();
        if let Some(scope) = scope {
            named.extend_from_slice(scope.as_bytes());
            named.extend_from_slice(b"::");
        }
        named.extend_from_slice(name);
        named
    }
}

/// What `observer_show_init` and `observer_show_init_backtrace` write
/// for a frame with no file.
const NO_ACTIVE_FILE: &[u8] = b"[no active file]";

/// `%*s` with php's width: two spaces a level.
fn indent(depth: usize) -> Vec<u8> {
    vec![b' '; depth.saturating_mul(2)]
}

/// `observer_fcall_init`: what is shown of a function or file on its
/// first call under `show_output`, and the handlers it is observed with:
/// every one under `observe_all`; a function under `observe_functions`
/// or where `observe_function_names` names it; a file under
/// `observe_includes`; none otherwise.
///
/// Under the compiled tier's force lane (`jit-design.md` §4.3,
/// *Observed calls*), a loop's calls are begun and ended as the
/// interpreter begins and ends them, a call the loop's trace inlines, a
/// native's and a silenced one among them; the child is served with the
/// switches on, which only startup sets, and php ignores the engine's
/// `jit.mode`:
///
/// ```php
/// $dir = sys_get_temp_dir() . '/observed' . getmypid(); @mkdir($dir);
/// file_put_contents("$dir/loop.php", '<?php
/// function f($x) { return $x + 1; }
/// function g($x) { return f($x) * 2; }
/// function gen() { yield 1; yield 2; }
/// function thrower($i) { if ($i == 1) { throw new Exception("x"); } return $i; }
/// $s = 0;
/// for ($i = 0; $i < 3; $i++) { $s = g($s) + strcmp("b", "a") + @f($i); }
/// for ($i = 0; $i < 3; $i++) { try { $s += thrower($i); } catch (Exception $e) { $s += 100; } }
/// foreach (gen() as $v) { $s += $v; }
/// echo $s, "\n";');
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=tracing', '-d', 'opcache.jit_buffer_size=64M',
///     '-d', 'jit.mode=force', '-d', 'zend_test.observer.enabled=1', '-d', 'zend_test.observer.observe_all=1',
///     '-d', 'zend_test.observer.show_return_value=1', "$dir/loop.php"], [1 => ['pipe', 'w']], $pipes);
/// echo str_replace(realpath($dir), 'D', stream_get_contents($pipes[1])); proc_close($p);
/// exec('rm -rf ' . escapeshellarg($dir));
/// ```
/// ```expect process regime=jit-force
/// <!-- init 'D/loop.php' -->
/// <file 'D/loop.php'>
///   <!-- init g() -->
///   <g>
///     <!-- init f() -->
///     <f>
///     </f:1>
///   </g:2>
///   <!-- init strcmp() -->
///   <strcmp>
///   </strcmp:1>
///   <f>
///   </f:1>
///   <g>
///     <f>
///     </f:5>
///   </g:10>
///   <strcmp>
///   </strcmp:1>
///   <f>
///   </f:2>
///   <g>
///     <f>
///     </f:14>
///   </g:28>
///   <strcmp>
///   </strcmp:1>
///   <f>
///   </f:3>
///   <!-- init thrower() -->
///   <thrower>
///   </thrower:0>
///   <thrower>
///     <!-- init Exception::__construct() -->
///     <Exception::__construct>
///     </Exception::__construct:NULL>
///     <!-- Exception: Exception -->
///   </thrower:NULL>
///   <thrower>
///   </thrower:2>
///   <!-- init gen() -->
///   <gen>
///   </gen:1>
///   <gen>
///   </gen:2>
///   <gen>
///   </gen:NULL>
/// 137
/// </file 'D/loop.php'>
/// ```
fn fcall_init(observed: &mut Observed<'_>) -> ObserverHandlers {
    let switches = Switches::of(observed);
    if !switches.enabled {
        return ObserverHandlers::default();
    }
    let serves = Serves::of(observed);
    if switches.show_output {
        show_init(observed, &serves);
        if switches.show_init_backtrace {
            show_init_backtrace(observed);
        }
        show_opcode(observed);
    }
    let both = ObserverHandlers {
        begin: Some(begin),
        end: Some(end),
    };
    let observed_by_name = |name: &[u8]| {
        names_in(observed.ini().get(OBSERVE_FUNCTION_NAMES)).any(|named| named == name)
    };
    match &serves {
        _ if switches.observe_all => both,
        Serves::Function { name, .. } if switches.observe_functions || observed_by_name(name) => {
            both
        }
        Serves::File(_) if switches.observe_includes => both,
        _ => ObserverHandlers::default(),
    }
}

/// The observer that stands for `zend_test_execute_internal`: a begin
/// handler on every native's call under `execute_internal`, and nothing
/// on a frame the loop serves, which php's pointer is never handed.
fn execute_internal_init(observed: &mut Observed<'_>) -> ObserverHandlers {
    if Switches::of(observed).execute_internal && observed.frame().is_none() {
        ObserverHandlers {
            begin: Some(internal_enter),
            end: None,
        }
    } else {
        ObserverHandlers::default()
    }
}

/// `zend_test_execute_internal` before it serves the native:
/// `<!-- internal enter f() -->` or `<!-- internal enter C::m() -->`.
fn internal_enter(observed: &mut Observed<'_>) {
    let Serves::Function { scope, name } = Serves::of(observed) else {
        return;
    };
    let mut line = indent(observed.state::<crate::State>().observer.depth);
    line.extend_from_slice(b"<!-- internal enter ");
    line.extend(Serves::function(scope.as_deref(), &name));
    line.extend_from_slice(b"() -->\n");
    observed.response_mut().write(&line);
}

/// `observer_show_init`: `<!-- init f() -->`, `<!-- init C::m() -->`
/// or `<!-- init 'file' -->`.
fn show_init(req: &mut Request, serves: &Serves) {
    let depth = req.state::<crate::State>().observer.depth;
    let mut line = indent(depth);
    line.extend_from_slice(b"<!-- init ");
    match serves {
        Serves::Function { scope, name } => {
            line.extend(Serves::function(scope.as_deref(), name));
            line.extend_from_slice(b"()");
        }
        Serves::File(file) => {
            line.push(b'\'');
            line.extend_from_slice(file);
            line.push(b'\'');
        }
    }
    line.extend_from_slice(b" -->\n");
    req.response_mut().write(&line);
}

/// `observer_show_init_backtrace`: the frame observed and every frame
/// below it on php's `prev_execute_data` chain ([`Observed::walk`]),
/// natives' calls and a fiber's bottom frame among them, `f()`,
/// `C::m()` or `{main} file`, four spaces in from the tag.
fn show_init_backtrace(observed: &mut Observed<'_>) {
    let depth = observed.state::<crate::State>().observer.depth;
    let mut out = indent(depth);
    out.extend_from_slice(b"<!--\n");
    for frame in observed.walk() {
        out.extend(indent(depth + 2));
        match Serves::chain(observed, &frame) {
            Serves::Function { scope, name } => {
                out.extend(Serves::function(scope.as_deref(), &name));
                out.extend_from_slice(b"()\n");
            }
            Serves::File(file) => {
                out.extend_from_slice(b"{main} ");
                out.extend_from_slice(&file);
                out.push(b'\n');
            }
        }
    }
    out.extend(indent(depth));
    out.extend_from_slice(b"-->\n");
    observed.response_mut().write(&out);
}

/// `observer_begin`: the interrupt raised under
/// `set_vm_interrupt_on_begin`, then, under `show_output`, the opening
/// tag, `<f>`, `<C::m>` or `<file 'name'>`, and one level deeper.
fn begin(observed: &mut Observed<'_>) {
    let switches = Switches::of(observed);
    if switches.set_vm_interrupt_on_begin {
        observed.limits().interrupt().raise();
    }
    if !switches.show_output {
        return;
    }
    let serves = Serves::of(observed);
    let state = &mut observed.state::<crate::State>().observer;
    let mut line = indent(state.depth);
    state.depth += 1;
    line.push(b'<');
    match &serves {
        Serves::Function { scope, name } => line.extend(Serves::function(scope.as_deref(), name)),
        Serves::File(file) => {
            line.extend_from_slice(b"file '");
            line.extend_from_slice(file);
            line.push(b'\'');
        }
    }
    line.extend_from_slice(b">\n");
    observed.response_mut().write(&line);
    show_opcode(observed);
}

/// `observer_end`: under `show_output`, the throwable standing where one
/// does, `<!-- Exception: C -->`, then one level out and the closing
/// tag, a function's with what it returned under `show_return_type` or
/// `show_return_value`.
fn end(observed: &mut Observed<'_>, returned: Option<&Value>) {
    let switches = Switches::of(observed);
    if !switches.show_output {
        return;
    }
    let depth = observed.state::<crate::State>().observer.depth;
    if let Some(raised) = observed.diagnostics().raised() {
        let class = match raised {
            Raised::Throwable(throwable) => {
                String::from_utf8_lossy(observed.symbols().class_name(throwable.class()))
                    .into_owned()
            }
            Raised::UnwindExit => String::from("UnwindExit"),
            Raised::GracefulExit => String::from("GracefulExit"),
        };
        let mut line = indent(depth);
        line.extend_from_slice(b"<!-- Exception: ");
        line.extend_from_slice(class.as_bytes());
        line.extend_from_slice(b" -->\n");
        observed.response_mut().write(&line);
    }
    show_opcode(observed);
    let depth = depth.saturating_sub(1);
    observed.state::<crate::State>().observer.depth = depth;
    let mut line = indent(depth);
    match Serves::of(observed) {
        Serves::Function { scope, name } => {
            line.extend_from_slice(b"</");
            line.extend(Serves::function(scope.as_deref(), &name));
            line.extend(retval_info(observed, switches, returned, depth));
        }
        Serves::File(file) => {
            line.extend_from_slice(b"</file '");
            line.extend_from_slice(&file);
            line.push(b'\'');
        }
    }
    line.extend_from_slice(b">\n");
    observed.response_mut().write(&line);
}

/// `get_retval_info`: nothing unless `show_return_type` or
/// `show_return_value` is on; then `:NULL` where the frame was left with
/// nothing, and otherwise under `show_return_value` an object as
/// `object(C)#n` and any other value as `var_export` writes it at
/// php's level, two more than the tag's indent and one, or under
/// `show_return_type` the type's name alone.
fn retval_info(
    req: &mut Request,
    switches: Switches,
    returned: Option<&Value>,
    depth: usize,
) -> Vec<u8> {
    if !switches.show_return_type && !switches.show_return_value {
        return Vec::new();
    }
    let mut info = vec![b':'];
    match returned {
        None => info.extend_from_slice(b"NULL"),
        Some(value) if switches.show_return_value => {
            if let Some(object) = value.as_object() {
                let named = [
                    b"object(".as_slice(),
                    req.symbols().class_name(object.class()),
                    format!(")#{}", object.index()).as_bytes(),
                ]
                .concat();
                info.extend_from_slice(&named);
            } else {
                let level = depth.saturating_mul(2).saturating_add(3);
                // What the walk threw stands pending, as php's smart_str
                // is left with what was written before it.
                php_var_export_ex(req, &mut info, value, level).ok();
            }
        }
        Some(value) => info.extend_from_slice(&type_name(req, value)),
    }
    info
}

/// `%p` of a fiber as php prints its `zend_fiber_context`: the `Fiber`'s
/// object number, and zero for the request's own stack,
/// `EG(main_fiber_context)`.
fn address(fiber: ObservedFiber) -> String {
    format!("{:#x}", fiber.fiber().map_or(0, |object| object.index()))
}

/// `fiber_init_observer`: `<!-- alloc: %p -->` under `fiber_init`.
fn fiber_init(req: &mut Request, fiber: ObservedFiber) {
    let switches = Switches::of(req);
    if switches.enabled && switches.fiber_init {
        req.response_mut()
            .write(format!("<!-- alloc: {} -->\n", address(fiber)).as_bytes());
    }
}

/// `fiber_destroy_observer`: `<!-- destroy: %p -->` under
/// `fiber_destroy`.
fn fiber_destroy(req: &mut Request, fiber: ObservedFiber) {
    let switches = Switches::of(req);
    if switches.enabled && switches.fiber_destroy {
        req.response_mut()
            .write(format!("<!-- destroy: {} -->\n", address(fiber)).as_bytes());
    }
}

/// The three switch observers php registers in turn, under
/// `fiber_switch`: `fiber_address_observer`, the switch itself;
/// `fiber_enter_observer`, the fiber entered where it starts, is
/// resumed or is resumed to be destroyed; and `fiber_suspend_observer`,
/// the fiber left where it is dead, by what it ended with, or
/// suspended.
fn fiber_switch(req: &mut Request, from: ObservedFiber, to: ObservedFiber) {
    let switches = Switches::of(req);
    if !(switches.enabled && switches.fiber_switch) {
        return;
    }
    let entered = if to.status() == FiberStatus::Init {
        Some("init")
    } else if !to.resumed_by(&from) {
        None
    } else if to.destroyed() {
        Some("destroying")
    } else {
        (to.status() != FiberStatus::Dead).then_some("resume")
    };
    let left = if from.status() == FiberStatus::Dead {
        Some(if from.threw() {
            "threw"
        } else if from.destroyed() {
            "destroyed"
        } else {
            "returned"
        })
    } else {
        (from.is_fiber() && !from.has_caller()).then_some("suspend")
    };
    let mut out = format!(
        "<!-- switching from fiber {} to {} -->\n",
        address(from),
        address(to)
    );
    for (tag, fiber) in [(entered, to), (left, from)] {
        if let Some(tag) = tag {
            writeln!(out, "<{tag} '{}'>", address(fiber)).ok();
        }
    }
    req.response_mut().write(out.as_bytes());
}
