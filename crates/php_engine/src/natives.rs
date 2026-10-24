//! The natives the engine declares itself (design §13): the Zend
//! builtins that need frames or the engine's own tables, which php-src
//! keeps in `ext/standard` and this engine keeps here, so an engine with
//! no extensions serves a program that uses Zend alone (design §4.10).
//!
//! Seventeen of them are here: the two the first witness of a loop needs,
//! the three that read what `php_heap::mm` accounts, so the allocator's
//! numbers are php's numbers under a witness and not under a terminal
//! session a person typed once, `ini_get` and `ini_set`, which read and
//! move the engine's own ini table so a witness can reach what an entry
//! decides and what it kept, `set_time_limit`, which moves the one
//! entry the execution timer is armed from, `error_reporting`, which
//! reads and writes the word `BEGIN_SILENCE` masks (#354), `strlen` and `defined`,
//! the two of `zend_builtin_functions.c` whose calls php's compiler
//! serves itself (#219): the compiler emits `ZEND_STRLEN` and
//! `ZEND_DEFINED` over their kernels where it reads the name, and reads
//! it only where the bundle declares the native; `class_exists` and
//! `get_class`, which read the linked table (#259); and
//! `register_shutdown_function`, whose entries the request holds and
//! [`call_shutdown_functions`] serves at `php_request_shutdown`'s first
//! step (#404); and `exit`, with `die` its second name, which throws
//! the unwind exit the unwinder tells apart (#423). Each is declared
//! and paired with its entry exactly as an
//! extension's are, so there is one shape for a native and not two.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ops::ControlFlow;

use php_macros::php_function;
use php_unit::decl::ClassKind;

use crate::api::abi::{
    Callable, Entry, Never, OrFalse, Passed, PropertyPurpose, Raw, Union, Variadic,
};
use crate::api::coerce::to_string;
use crate::api::raising::{Thrown, arg_type_error, argument_error, deprecated, error, warning};
use crate::api::request::Request;
use crate::api::values::walk::{self, GiveUp, Hold, Unwritten};
use crate::api::values::{
    Key, Laziness, Owner, PhpArray, PhpObject, PhpString, RecursionGuard, Value, ValueRef,
};
use crate::c_number::c_str;
use php_unit::decl::FunctionDecl;

mod backtrace;
mod classes;
mod errors;
mod functions;

pub(crate) use functions::{ZEND_VERSION, copied_arguments};

/// Where one of [`standard`]'s natives stands among `ext/standard`'s
/// own in `basic_functions.stub.php`, which is the order
/// `get_extension_funcs('standard')` and `php --re standard` list them
/// in: first, or right after the function the stub declares before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StubPlace {
    /// The stub's first function.
    First,
    /// Right after this function of the stub, whichever of the engine
    /// or `ext/standard` declares it.
    After(&'static str),
}

impl StubPlace {
    /// Whether a native at this place goes right after `last`, the
    /// function placed before it so far, none at the start of the list.
    pub(crate) fn follows(self, last: Option<&FunctionDecl>) -> bool {
        match (self, last) {
            (Self::First, None) => true,
            (Self::After(name), Some(decl)) => decl.name.0.eq_ignore_ascii_case(name.as_bytes()),
            _ => false,
        }
    }
}

/// The natives php keeps in `ext/standard` and this engine here, each
/// at its place in `basic_functions.stub.php`. Where the bundle has
/// `ext/standard` they are its, spliced among its own at those places
/// (`internal_function.module` naming it); an engine without it lists
/// them after Core's under no extension.
///
/// ```php
/// var_dump((new ReflectionFunction('var_dump'))->getExtensionName());
/// var_dump((new ReflectionFunction('ini_get'))->getExtensionName());
/// $f = get_extension_funcs('standard');
/// foreach (['set_time_limit', 'forward_static_call_array', 'register_shutdown_function', 'highlight_file', 'highlight_string', 'ini_get', 'ini_get_all', 'ini_set', 'ini_alter', 'ini_restore', 'var_dump', 'var_export', 'unserialize', 'memory_get_usage', 'memory_get_peak_usage', 'memory_reset_peak_usage', 'version_compare'] as $name) {
///     echo $name, ' ', array_search($name, $f) - array_search('set_time_limit', $f), "\n";
/// }
/// ```
/// ```expect
/// string(8) "standard"
/// string(8) "standard"
/// set_time_limit 0
/// forward_static_call_array 129
/// register_shutdown_function 130
/// highlight_file 131
/// highlight_string 134
/// ini_get 135
/// ini_get_all 136
/// ini_set 137
/// ini_alter 138
/// ini_restore 139
/// var_dump 532
/// var_export 533
/// unserialize 536
/// memory_get_usage 537
/// memory_get_peak_usage 538
/// memory_reset_peak_usage 539
/// version_compare 540
/// ```
pub(crate) fn standard() -> Vec<(StubPlace, (FunctionDecl, Entry))> {
    vec![
        (StubPlace::First, set_time_limit::declared()),
        (
            StubPlace::After("forward_static_call_array"),
            register_shutdown_function::declared(),
        ),
        (StubPlace::After("highlight_string"), ini_get::declared()),
        (StubPlace::After("ini_get_all"), ini_set::declared()),
        (StubPlace::After("ini_set"), ini_alter::declared()),
        (StubPlace::After("convert_uudecode"), var_dump::declared()),
        (
            StubPlace::After("unserialize"),
            memory_get_usage::declared(),
        ),
        (
            StubPlace::After("memory_get_usage"),
            memory_get_peak_usage::declared(),
        ),
        (
            StubPlace::After("memory_get_peak_usage"),
            memory_reset_peak_usage::declared(),
        ),
    ]
}

/// Core's natives, `zend_builtin_functions.stub.php`'s functions in its
/// order, which is the order `get_defined_functions()` and
/// `get_extension_funcs('Core')` list them in; `zend_thread_id`, which a
/// build without `ZEND_DEBUG` and `ZTS` does not declare, left out.
pub(crate) fn core() -> Vec<(FunctionDecl, Entry)> {
    vec![
        clone::declared(),
        exit::declared(),
        die::declared(),
        functions::zend_version::declared(),
        functions::func_num_args::declared(),
        functions::func_get_arg::declared(),
        functions::func_get_args::declared(),
        strlen::declared(),
        functions::strcmp::declared(),
        functions::strncmp::declared(),
        functions::strcasecmp::declared(),
        functions::strncasecmp::declared(),
        error_reporting::declared(),
        define::declared(),
        defined::declared(),
        get_class::declared(),
        classes::get_called_class::declared(),
        classes::get_parent_class::declared(),
        classes::is_subclass_of::declared(),
        classes::is_a::declared(),
        classes::get_class_vars::declared(),
        classes::get_object_vars::declared(),
        classes::get_mangled_object_vars::declared(),
        classes::get_class_methods::declared(),
        classes::method_exists::declared(),
        classes::property_exists::declared(),
        class_exists::declared(),
        classes::interface_exists::declared(),
        classes::trait_exists::declared(),
        classes::enum_exists::declared(),
        functions::function_exists::declared(),
        classes::class_alias::declared(),
        functions::get_included_files::declared(),
        functions::get_required_files::declared(),
        errors::trigger_error::declared(),
        errors::user_error::declared(),
        errors::set_error_handler::declared(),
        errors::restore_error_handler::declared(),
        errors::get_error_handler::declared(),
        errors::set_exception_handler::declared(),
        errors::restore_exception_handler::declared(),
        errors::get_exception_handler::declared(),
        classes::get_declared_classes::declared(),
        classes::get_declared_traits::declared(),
        classes::get_declared_interfaces::declared(),
        functions::get_defined_functions::declared(),
        functions::get_defined_vars::declared(),
        functions::get_resource_type::declared(),
        functions::get_resource_id::declared(),
        functions::get_resources::declared(),
        functions::get_loaded_extensions::declared(),
        functions::get_defined_constants::declared(),
        backtrace::debug_backtrace::declared(),
        backtrace::debug_print_backtrace::declared(),
        extension_loaded::declared(),
        functions::get_extension_funcs::declared(),
        gc_mem_caches::declared(),
        gc_collect_cycles::declared(),
        gc_enabled::declared(),
        gc_enable::declared(),
        gc_disable::declared(),
        gc_status::declared(),
    ]
}

/// `gc_mem_caches(): int`, `zend_mm_gc`: the bin `runs` wholly free
/// given back to their chunks, and the bytes they held. Four hundred
/// strings of 3000 bytes are a hundred `runs` of three pages of the
/// 3072-byte bin; given back, their pages are free again, and none of
/// them is swept twice.
///
/// ```php
/// $s = [];
/// for ($i = 0; $i < 400; $i++) { $s[] = str_repeat("x", 3000) . $i; }
/// $s = null;
/// var_dump(gc_mem_caches() >= 100 * 12288, gc_mem_caches());
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// ```
#[php_function]
fn gc_mem_caches(req: &mut Request) -> i64 {
    as_long(req.heap_mut().gc_mem_caches())
}

/// `gc_collect_cycles(): int`, `zend_gc_collect_cycles`: a collection over the
/// root buffer, and the values it freed, strings and resources not
/// among them since php 8.5. Two objects that name each other are
/// garbage once their variables are gone: their destructors are served
/// first, in the order the walk found them, the collection is repeated once
/// after them, and both are freed, so the collection counts two.
///
/// ```php
/// class Node {
///     public $other;
///     public function __construct(public $name) {}
///     public function __destruct() { echo "destruct {$this->name}\n"; }
/// }
/// $a = new Node('a');
/// $b = new Node('b');
/// $a->other = $b;
/// $b->other = $a;
/// unset($a, $b);
/// echo "before\n";
/// var_dump(gc_collect_cycles());
/// echo "after\n";
/// var_dump(gc_collect_cycles());
/// ```
/// ```expect
/// before
/// destruct a
/// destruct b
/// int(2)
/// after
/// int(0)
/// ```
///
/// A destructor that gives its object an owner resurrects it: the collection
/// after the destructors finds it live and frees nothing, and once that
/// owner lets go the cycle is garbage again and its destructor, served
/// once, is not served a second time.
///
/// ```php
/// class Keeper {
///     public static $kept;
///     public $self;
///     public function __destruct() { echo "destruct\n"; Keeper::$kept = $this; }
/// }
/// $k = new Keeper;
/// $k->self = $k;
/// unset($k);
/// var_dump(gc_collect_cycles(), Keeper::$kept instanceof Keeper);
/// Keeper::$kept = null;
/// var_dump(gc_collect_cycles());
/// ```
/// ```expect
/// destruct
/// int(0)
/// bool(true)
/// int(1)
/// ```
#[php_function]
fn gc_collect_cycles(req: &mut Request) -> i64 {
    req.gc_collect()
}

/// `gc_enabled(): bool`: whether a full root buffer runs the collector,
/// which `zend.enable_gc` sets.
///
/// ```php
/// var_dump(gc_enabled(), ini_get('zend.enable_gc'));
/// gc_disable();
/// var_dump(gc_enabled(), ini_get('zend.enable_gc'));
/// gc_enable();
/// var_dump(gc_enabled(), ini_get('zend.enable_gc'));
/// ini_set('zend.enable_gc', 'off');
/// var_dump(gc_enabled());
/// ```
/// ```expect
/// bool(true)
/// string(1) "1"
/// bool(false)
/// string(1) "0"
/// bool(true)
/// string(1) "1"
/// bool(false)
/// ```
#[php_function]
fn gc_enabled(req: &mut Request) -> bool {
    req.gc_enabled()
}

/// `gc_enable(): void`, `zend.enable_gc` altered to `1`. The collector
/// is only turned off by `gc_disable()`: roots are still added to the
/// buffer, and `gc_collect_cycles()` still runs, but a full buffer runs
/// nothing.
///
/// ```php
/// gc_disable();
/// $a = [];
/// $a[] =& $a;
/// unset($a);
/// var_dump(gc_status()['roots'], gc_collect_cycles());
/// gc_enable();
/// var_dump(gc_enabled());
/// ```
/// ```expect
/// int(1)
/// int(1)
/// bool(true)
/// ```
#[php_function]
fn gc_enable(req: &mut Request) {
    req.gc_enable(true);
}

/// `gc_disable(): void`, `zend.enable_gc` altered to `0`. With the
/// collector off, ten thousand cycles fill the buffer past its
/// threshold and no collection is made; `gc_collect_cycles()` frees them all.
///
/// ```php
/// gc_disable();
/// for ($i = 0; $i < 10001; $i++) { $o = new stdClass; $o->o = $o; }
/// unset($o);
/// $s = gc_status();
/// var_dump($s['runs'], $s['roots'], gc_collect_cycles());
/// ```
/// ```expect
/// int(0)
/// int(10001)
/// int(10001)
/// ```
#[php_function]
fn gc_disable(req: &mut Request) {
    req.gc_enable(false);
}

/// `gc_status(): array`, `zend_gc_get_status` in php 8.5's twelve
/// fields. A table that holds itself by reference is one root once its
/// variable is gone, and one value collected by the collection that finds it.
///
/// ```php
/// $a = [];
/// $a[] =& $a;
/// unset($a);
/// $s = gc_status();
/// var_dump(array_keys($s), $s['runs'], $s['roots'], $s['threshold'], $s['buffer_size']);
/// gc_collect_cycles();
/// $s = gc_status();
/// var_dump($s['running'], $s['protected'], $s['full'], $s['runs'], $s['collected'], $s['roots']);
/// var_dump(is_float($s['application_time']), is_float($s['collector_time']));
/// ```
/// ```expect
/// array(12) {
///   [0]=>
///   string(7) "running"
///   [1]=>
///   string(9) "protected"
///   [2]=>
///   string(4) "full"
///   [3]=>
///   string(4) "runs"
///   [4]=>
///   string(9) "collected"
///   [5]=>
///   string(9) "threshold"
///   [6]=>
///   string(11) "buffer_size"
///   [7]=>
///   string(5) "roots"
///   [8]=>
///   string(16) "application_time"
///   [9]=>
///   string(14) "collector_time"
///   [10]=>
///   string(15) "destructor_time"
///   [11]=>
///   string(9) "free_time"
/// }
/// int(0)
/// int(1)
/// int(10001)
/// int(16384)
/// bool(false)
/// bool(false)
/// bool(false)
/// int(1)
/// int(1)
/// int(0)
/// bool(true)
/// bool(true)
/// ```
///
/// A full buffer runs the collector on its own: the ten thousand and
/// first root is one past the threshold, and the collection it starts frees
/// the ten thousand cycles and leaves the last root in the buffer.
///
/// ```php
/// for ($i = 0; $i < 10001; $i++) { $o = new stdClass; $o->o = $o; }
/// unset($o);
/// $s = gc_status();
/// var_dump($s['runs'], $s['collected'], $s['roots'], $s['threshold']);
/// ```
/// ```expect
/// int(1)
/// int(10000)
/// int(1)
/// int(10001)
/// ```
#[php_function]
fn gc_status(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    req.gc_status()
}

/// `clone(object $object, array $withProperties = []): object`, php
/// 8.5's `ZEND_FUNCTION(clone)` (`Z/Zend/zend_builtin_functions.c:72`),
/// kept in step with `ZEND_CLONE`, which the compiler emits for a call
/// with one argument: the class's `clone_obj` (design §5.4) through
/// [`Request::clone_object`], or its `clone_obj_with` where
/// `$withProperties` names any, the copy the caller's with one owner.
/// A first argument that is no object is php's `TypeError`.
///
/// ```php
/// class C { public $p = 1; }
/// $c = new C; $d = clone($c);
/// var_dump($d->p, $c === $d, $d instanceof C);
/// ```
/// ```expect
/// int(1)
/// bool(false)
/// bool(true)
/// ```
///
/// A call the compiler cannot make `ZEND_CLONE` of reaches the native:
/// by a name held in a variable, and with the properties to write.
///
/// ```php
/// class C { public $p = 1; }
/// $c = new C; $f = 'clone';
/// $d = $f($c);
/// $e = $f($c, ['p' => 2]);
/// var_dump($d->p, $c === $d, $e->p, $c->p);
/// ```
/// ```expect
/// int(1)
/// bool(false)
/// int(2)
/// int(1)
/// ```
///
/// A readonly property is written once more by the copy's own class,
/// php's wither, and refused from a scope that may not set it:
///
/// ```php
/// final class Point {
///     public function __construct(public readonly int $x, public readonly int $y) {}
///     public function withX(int $x): static { return clone($this, ['x' => $x]); }
/// }
/// $p = new Point(1, 2);
/// $q = $p->withX(3);
/// var_dump($q->x, $q->y, $p->x);
/// try { $q->x = 4; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { clone($p, ['x' => 'no']); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// int(2)
/// int(1)
/// Cannot modify readonly property Point::$x
/// Cannot modify protected(set) readonly property Point::$x from global scope
/// ```
#[php_function]
fn clone(
    req: &mut Request,
    object: PhpObject,
    #[php(default = [])] withProperties: Passed<PhpArray>,
) -> Result<Owner<PhpObject>, Thrown> {
    let with = match withProperties {
        Passed::Given(with) => Some(with),
        Passed::Omitted => None,
    };
    req.clone_object(object, with)
}

/// `exit(string|int $status = 0): never`, `ZEND_FUNCTION(exit)`
/// (`Z/Zend/zend_builtin_functions.c:118`), which `die` is a second
/// name for: one `Z_PARAM_STR_OR_LONG` and then
/// [`crate::api::raising::exit`], a string written and an int made
/// php's `EG(exit_status)`, and the unwind exit thrown. Bare `exit` and
/// `die` are the same call with no argument, as php's parser makes them.
///
/// ```php
/// echo "a\n";
/// exit(3);
/// echo "b\n";
/// ```
/// ```expect exit=3
/// a
/// ```
///
/// ```php
/// echo "a\n";
/// die("bye\n");
/// ```
/// ```expect
/// a
/// bye
/// ```
///
/// ```php
/// echo "a\n";
/// exit;
/// ```
/// ```expect
/// a
/// ```
///
/// ```php
/// echo "a\n";
/// die;
/// ```
/// ```expect
/// a
/// ```
///
/// The parameter is `string|int`, so a bool is its int, a float is
/// read as an int where it fits, behind the 8.1 deprecation where that
/// loses something, and as its string where it does not, `null` is the
/// 8.1 deprecation and then `0`, an object with `__toString` is that
/// string, and what is none of these is php's `TypeError`; `exit(300)`
/// is the status the process reports, which the OS masks to 44. A
/// second argument is php's `ArgumentCountError`.
///
/// ```php
/// exit(3.5);
/// ```
/// ```expect exit=3
/// Deprecated: Implicit conversion from float 3.5 to int loses precision in %s on line 2
/// ```
///
/// ```php
/// exit(1e19);
/// ```
/// ```expect
/// 1.0E+19
/// ```
///
/// ```php
/// exit(null);
/// ```
/// ```expect
/// Deprecated: exit(): Passing null to parameter #1 ($status) of type string|int is deprecated in %s on line 2
/// ```
///
/// ```php
/// exit(true);
/// ```
/// ```expect exit=1
/// ```
///
/// ```php
/// class S { function __toString() { return "s\n"; } }
/// exit(new S);
/// ```
/// ```expect
/// s
/// ```
///
/// ```php
/// exit([]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: exit(): Argument #1 ($status) must be of type string|int, array given in %s:2
/// Stack trace:
/// #0 %s(2): exit(Array)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// exit(0, 1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: exit() expects at most 1 argument, 2 given in %s:2
/// Stack trace:
/// #0 %s(2): exit(0, 1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// A native whose callback exits reads the unwind exit as it reads a
/// throwable the callback left, and returns with it standing, so the
/// unwinder goes on from the call: what follows the call is never
/// served, and php's status is the callback's.
///
/// ```php
/// call_user_func(function () { echo "in\n"; exit(7); });
/// echo "after\n";
/// ```
/// ```expect exit=7
/// in
/// ```
///
/// The status is `EG(exit_status)`, one word the last write wins, so
/// an `exit` in a destructor served at shutdown moves what the main
/// op array's `exit` set, and a fatal there is rendered and moves it
/// to 255.
///
/// ```php
/// class A { function __destruct() { echo "d\n"; exit(3); } }
/// $a = new A;
/// exit(1);
/// ```
/// ```expect exit=3
/// d
/// ```
///
/// ```php
/// class A { function __destruct() { nope(); } }
/// $a = new A;
/// exit(1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to undefined function nope() in %s:2
/// Stack trace:
/// #0 [internal function]: A->__destruct()
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// A shutdown function's `exit` ends the shutdown functions where php's
/// `zend_bailout` does, after the status it set; one after a fatal
/// moves the status the fatal set.
///
/// ```php
/// register_shutdown_function(function () { echo "s\n"; exit(4); });
/// register_shutdown_function(function () { echo "s2\n"; });
/// exit(1);
/// ```
/// ```expect exit=4
/// s
/// ```
///
/// ```php
/// register_shutdown_function(function () { echo "s\n"; exit(4); });
/// nope();
/// ```
/// ```expect exit=4
/// Fatal error: Uncaught Error: Call to undefined function nope() in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// s
/// ```
///
/// A destructor's throwable during the unwind takes the unwind exit's
/// place, as `zend_exception_set_previous` releases the exit where it
/// would chain it: the request ends in that throwable, uncaught, and
/// the `catch` the `exit` stood in never sees it, since the CV is
/// destroyed when the frame is left.
///
/// ```php
/// class A { function __destruct() { throw new Exception("d"); } }
/// try { $a = new A; exit(9); } catch (Exception $e) { echo "c\n"; }
/// echo "after\n";
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Exception: d in %s:2
/// Stack trace:
/// #0 [internal function]: A->__destruct()
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function(aliases(die))]
fn exit(
    req: &mut Request,
    #[php(default = 0)] status: Union<PhpString, i64>,
) -> Result<Never, Thrown> {
    let status = match status {
        Union::A(string) => Value::string(string),
        Union::B(code) => Value::int(code),
    };
    Err(crate::api::raising::exit(req, &status))
}

/// `class_exists(string $class, bool $autoload = true): bool`, php's
/// `class_exists_impl` (`Z/Zend/zend_builtin_functions.c:1136`) with
/// `ZEND_ACC_LINKED` to have and `ZEND_ACC_INTERFACE | ZEND_ACC_TRAIT`
/// to lack: the class under the lowered name, a leading `\` dropped,
/// which is a class or an enum and not an interface or a trait. The
/// autoloaders are served for a miss under `$autoload`, and none is
/// registered yet. Declared frameless for one argument and for two
/// (`frameless_function_infos_class_exists`), so a call of either is a
/// `FRAMELESS_ICALL_1` or `_2` over this body, and a refusal's trace
/// has php's frameless row.
///
/// ```php
/// class M {}
/// var_dump(class_exists('M'), class_exists('m'), class_exists('\M'), class_exists('Exception'), class_exists('Throwable'), class_exists('Nope'), class_exists('Nope', false));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// $a = []; try { class_exists($a); } catch (TypeError $e) { echo $e->getMessage(), "\n", implode(',', array_keys($e->getTrace()[0])), "\n"; }
/// var_dump(class_exists('stdClass', false));
/// ```
/// ```expect
/// class_exists(): Argument #1 ($class) must be of type string, array given
/// function,file,line,args
/// bool(true)
/// ```
///
/// An autoloader's throw under it peaks where php's does, the call's
/// row in the throwable's trace php's frameless one:
///
/// ```php
/// spl_autoload_register(function ($c) { throw new RuntimeException('boom'); });
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
/// pk(function () { try { class_exists('Boom'); } catch (Exception $e) {} });
/// ```
/// ```expect
/// 3096 32
/// ```
#[php_function(frameless(1, 2))]
fn class_exists(
    req: &mut Request,
    class: PhpString,
    #[php(default = true)] autoload: bool,
) -> Result<bool, Thrown> {
    classes::exists(req, class, autoload, &[ClassKind::Class, ClassKind::Enum])
}

/// `get_class(object $object = UNKNOWN): string`, php's
/// `ZEND_FUNCTION(get_class)` (`Z/Zend/zend_builtin_functions.c:721`):
/// the declared name of the object's class, `ce->name`; without an
/// argument the executed scope's name, deprecated since 8.3, and php's
/// `Error` where there is no scope. The scope is
/// `zend_get_executed_scope`'s, which walks past this native's own
/// frame to the method that called it. php's compiler serves a call of
/// none or one argument with `ZEND_GET_CLASS`
/// (`zend_compile_func_get_class`); what reaches the native is a call by
/// a name the program holds, a named or an unpacked argument, and a
/// callback.
///
/// ```php
/// class M { function f() { $g = 'get_class'; return $g(); } }
/// var_dump((new M)->f());
/// ```
/// ```expect
/// Deprecated: Calling get_class() without arguments is deprecated in %s on line 2
/// string(1) "M"
/// ```
///
/// ```php
/// class M {} $m = new M; $g = 'get_class';
/// var_dump($g($m), $g(new Exception));
/// try { $g(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "M"
/// string(9) "Exception"
/// get_class(): Argument #1 ($object) must be of type object, int given
/// ```
///
/// ```php
/// $g = 'get_class'; try { $g(); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Error: get_class() without arguments must be called from within a class
/// ```
#[php_function]
fn get_class(req: &mut Request, object: Passed<PhpObject>) -> Result<Owner<PhpString>, Thrown> {
    let class = if let Passed::Given(object) = object {
        object.class()
    } else {
        match req.frames().scope() {
            Some(scope) => {
                deprecated(req, "Calling get_class() without arguments is deprecated");
                scope
            }
            None => {
                return Err(error(
                    req,
                    "get_class() without arguments must be called from within a class",
                ));
            }
        }
    };
    let name = req.class_name_bytes(class);
    req.declared_string(&name)
}

/// `strlen(string $string): int`, php's `ZEND_FUNCTION(strlen)`
/// (`Z/Zend/zend_builtin_functions.c:374`): one `Z_PARAM_STR` and its
/// byte length. A call of one argument never reaches here, since the
/// compiler emits `ZEND_STRLEN` over [`crate::ops::strlen`] for it
/// (`zend_compile_func_strlen`); what does is a call of any other
/// count, which `ZEND_PARSE_PARAMETERS_START(1, 1)` refuses, and the
/// native's frame is on the trace where the opcode's is not.
///
/// ```php
/// var_dump(strlen("a", "b"));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: strlen() expects exactly 1 argument, 2 given in %s:2
/// Stack trace:
/// #0 %s(2): strlen('a', 'b')
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn strlen(string: PhpString) -> i64 {
    as_long(string.len())
}

/// `define(string $constant_name, mixed $value, bool $case_insensitive
/// = false): bool`, php's `ZEND_FUNCTION(define)`
/// (`Z/Zend/zend_builtin_functions.c:555`) over `zend_register_constant`
/// (`Z/Zend/zend_constants.c:511`). The arguments are read first, so a
/// third that is no bool is its `TypeError` before anything else; a
/// name with `::` in it is the `ValueError`; a third argument that is
/// true is the 8.0 warning that it is ignored. An array value is
/// walked for a reference back into itself, the `ValueError` for a
/// recursive array, and copied with every reference in it followed
/// (`copy_constant_array`) where it is refcounted; an immutable array,
/// one the unit holds, is kept as it stands, and every other value
/// takes one more owner. The
/// name's namespace, the bytes before its last `\`, is lowered, as the
/// table is keyed; `true`, `false`, `null` in any case,
/// `__COMPILER_HALT_OFFSET__` and a name the table has are the
/// warning that the constant is defined already, and `false`. What is
/// written is what `constant_named` answers, so `defined()`, a
/// `FETCH_CONSTANT` and `ReflectionConstant` read it from here on.
///
/// ```php
/// var_dump(define("DEMO", "f\0oo"), defined("DEMO"), define("F\0oo", true), defined("F\0oo"), define("\\ns\\Y", 1), defined("NS\\Y"), define("A\\b\\C", 1), defined("a\\b\\C"), define("Z", new stdClass), define("W", [1, [2]]), defined("W"));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
///
/// ```php
/// var_dump(define("X", 1, true), define("X", 2), define("Null", 1), define("__COMPILER_HALT_OFFSET__", 1), define("PHP_EOL", 1));
/// ```
/// ```expect
/// Warning: define(): Argument #3 ($case_insensitive) is ignored since declaration of case-insensitive constants is no longer supported in %s on line 2
///
/// Warning: Constant X already defined, this will be an error in PHP 9 in %s on line 2
///
/// Warning: Constant Null already defined, this will be an error in PHP 9 in %s on line 2
///
/// Warning: Constant __COMPILER_HALT_OFFSET__ already defined, this will be an error in PHP 9 in %s on line 2
///
/// Warning: Constant PHP_EOL already defined, this will be an error in PHP 9 in %s on line 2
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// An array the unit folded is defined as it stands, and a nested one is kept
/// in the copy of an array of the request's:
///
/// ```php
/// define('H', ['h']); debug_zval_dump(H);
/// $a = [['x']]; $a[] = 2; define('N', $a); debug_zval_dump(N);
/// ```
/// ```expect regime=immutable-literals
/// array(1) packed interned {
///   [0]=>
///   string(1) "h" interned
/// }
/// array(2) packed refcount(2){
///   [0]=>
///   array(1) packed interned {
///     [0]=>
///     string(1) "x" interned
///   }
///   [1]=>
///   int(2)
/// }
/// ```
///
/// ```php
/// define("A::B", 1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ValueError: define(): Argument #1 ($constant_name) cannot be a class constant in %s:2
/// Stack trace:
/// #0 %s(2): define('A::B', 1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// define("X");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: define() expects at least 2 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): define('X')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// A class constant's name is `zend_argument_value_error`'s, and peaks
/// where php's does:
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
/// pk(function () { try { define('A::B', 1); } catch (ValueError $e) {} });
/// ```
/// ```expect
/// 2920 32
/// ```
///
/// The constant is listed under its name as given and found whatever
/// its namespace's case, as `zend_register_constant` keys it by the name
/// with its namespace lowered and keeps `c->name` as written:
///
/// ```php
/// define("NS1\\ns2\\Const1", "value1");
/// var_dump(array_keys(get_defined_constants(true)["user"]), constant("ns1\\NS2\\Const1"), defined("NS1\\ns2\\const1"));
/// ```
/// ```expect exit=0
/// array(1) {
///   [0]=>
///   string(14) "NS1\ns2\Const1"
/// }
/// string(6) "value1"
/// bool(false)
/// ```
#[php_function]
fn define(
    req: &mut Request,
    constant_name: PhpString,
    value: Value,
    #[php(default = false)] case_insensitive: bool,
) -> Result<bool, Thrown> {
    let bytes = constant_name.as_bytes();
    if bytes.windows(2).any(|pair| pair == b"::") {
        return Err(argument_error(
            req,
            "ValueError",
            1,
            "cannot be a class constant",
        ));
    }
    if case_insensitive {
        warning(
            req,
            "define(): Argument #3 ($case_insensitive) is ignored since declaration of case-insensitive constants is no longer supported",
        );
    }
    let value = if let Some(array) = value.as_array()
        && value.is_refcounted()
    {
        if !constant_array_is_acyclic(req, array)? {
            return Err(argument_error(
                req,
                "ValueError",
                2,
                "cannot be a recursive array",
            ));
        }
        copy_constant_array(req, array)?.into_value()
    } else {
        Owner::take(req, value)
    };
    Ok(req.define_constant(bytes, value))
}

/// `validate_constant_array_argument`: no array in the value, through
/// any reference, is one the walk is already inside.
/// The walk, php's unguarded, is held to the thread's floor
/// ([`crate::api::raising::call_stack_end`]), as the copy after it is.
fn constant_array_is_acyclic(req: &mut Request, array: PhpArray) -> Result<bool, Thrown> {
    if array.is_recursive() {
        return Ok(false);
    }
    crate::api::raising::call_stack_end(req)?;
    array.protect();
    let mut acyclic = Ok(true);
    // The nested arrays read under the borrowed request: the walk runs no
    // code of the program, and the array, being a constant's value, is
    // held by what defines it.
    let nested: Vec<PhpArray> = walk::rows(req, array)
        .filter_map(|(_, v)| v.deref().as_array())
        .collect();
    for nested in nested {
        acyclic = constant_array_is_acyclic(req, nested);
        if !matches!(acyclic, Ok(true)) {
            break;
        }
    }
    array.unprotect();
    acyclic
}

/// `copy_constant_array`: a new array of the request's with the same
/// keys, each value with its reference followed, a nested array copied
/// the same way where it is refcounted, and everything else, an
/// immutable array among it, one more owner.
fn copy_constant_array(req: &mut Request, array: PhpArray) -> Result<Owner<PhpArray>, Thrown> {
    crate::api::raising::call_stack_end(req)?;
    let mut copy = PhpArray::new(req)?;
    // Read under the borrowed request, as the check before it reads them:
    // the copy runs no code of the program.
    let rows: Vec<(Key, Value)> = walk::rows(req, array)
        .map(|(key, value)| (key, value.deref()))
        .collect();
    for (key, value) in rows {
        let value = if let Some(nested) = value.as_array()
            && value.is_refcounted()
        {
            copy_constant_array(req, nested)?.into_value()
        } else {
            Owner::take(req, value)
        };
        let key = Owner::take(req, key);
        copy.set(req, key, value)?;
    }
    Ok(copy)
}

/// `defined(string $constant_name): bool`, php's `ZEND_FUNCTION(defined)`
/// (`Z/Zend/zend_builtin_functions.c:608`), which is
/// `zend_get_constant_ex` under `ZEND_FETCH_CLASS_SILENT`: a leading
/// `\` is dropped; a `::` names a class constant, answered by
/// `zend_get_class_constant_ex` as silently as `ZEND_FETCH_CLASS_SILENT`
/// asks, the class
/// autoloaded and nothing raised for one missing, one the scope may not
/// reach or one `#[\Deprecated]` marks; a `\`
/// left in the name is a namespaced constant, looked up with the
/// namespace lowered and the name as written, in the table alone; and
/// anything else is [`crate::ops::defined`], the table and then `true`,
/// `false` and `null` in any case. The compiler emits `ZEND_DEFINED`
/// where the name is written as a string with neither separator in it
/// (`zend_compile_func_defined`), so what reaches here is a name it
/// could not read, with a separator, or not a string, which `Z_PARAM_STR`
/// coerces as it coerces every string parameter.
///
/// ```php
/// var_dump(defined(null), defined("\\"), defined("Foo\\PHP_VERSION"), defined("\\PHP_VERSION"), defined("A::B"), defined("foo\\PHP_VERSION"), defined("\\Foo\\BAR"), defined("\\TRUE"), defined("x:y"), defined(":E_ALL"), defined("::E_ALL"));
/// ```
/// ```expect
/// Deprecated: defined(): Passing null to parameter #1 ($constant_name) of type string is deprecated in %s on line 2
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// class C { #[\Deprecated] const D = 1; private const P = 2; }
/// var_dump(defined('C::D'), defined('C::P'), defined('C::nope'), defined('Nope::D'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// var_dump(defined(token_get_all("<?php 1;")));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: defined(): Argument #1 ($constant_name) must be of type string, array given in %s:2
/// Stack trace:
/// #0 %s(2): defined(Array)
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn defined(req: &mut Request, constant_name: PhpString) -> Result<bool, Thrown> {
    let mut bytes = constant_name.as_bytes();
    if let Some(rest) = bytes.strip_prefix(b"\\") {
        bytes = rest;
    }
    if let Some(colon) = bytes.iter().rposition(|b| *b == b':')
        && colon > 0
        && bytes.get(colon - 1) == Some(&b':')
    {
        let (class_name, constant) = (&bytes[..colon - 1], &bytes[colon + 1..]);
        let scoped = [&b"self"[..], b"parent", b"static"]
            .iter()
            .any(|word| class_name.eq_ignore_ascii_case(word));
        let class = if scoped {
            Some(crate::api::call::fetch_class(req, class_name)?)
        } else {
            crate::api::call::autoload(req, class_name)?
        };
        return Ok(class.is_some_and(|class| req.class_constant_defined(class, constant)));
    }
    if bytes.contains(&b'\\') {
        return Ok(req.constant_named(&Request::constant_key(bytes)).is_some());
    }
    Ok(crate::ops::defined(req, bytes))
}

/// `var_dump(mixed $value, mixed ...$values): void`, php's
/// `php_var_dump`: each value's type and contents, one line each, and an
/// array as `php_array_element_dump` lays it out, each element's key on
/// its own line and the value indented two more than the key, so a nested
/// array steps in by two each level. No user code builds an array yet; a
/// native's answer is one, and this is how a witness reads it
/// (`ext/tokenizer`'s `token_get_all` is the first).
///
/// ```php
/// var_dump(true, false, null, 1, -2, 2.5, 0.1, "demo", extension_loaded('Core'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// NULL
/// int(1)
/// int(-2)
/// float(2.5)
/// float(0.1)
/// string(4) "demo"
/// bool(true)
/// ```
///
/// A throwable a native leaves pending takes the native's call as its
/// throw site, which php reads off the caller's opline; uncaught, it
/// names that site twice and carries the native's own frame above
/// `{main}`.
///
/// ```php
/// var_dump();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: var_dump() expects at least 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): var_dump()
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// An object is `php_var_dump`'s `IS_OBJECT` arm: its class, its
/// store index, its properties in declaration order with the
/// visibility php mangles into a name, an untyped property nothing
/// wrote as `NULL`, and the dynamic ones after.
///
/// ```php
/// class M { public $x = 1; public $y; protected $p = 'p'; private $q = 2.5; }
/// class N {}
/// var_dump(new M, new N);
/// ```
/// ```expect
/// object(M)#1 (4) {
///   ["x"]=>
///   int(1)
///   ["y"]=>
///   NULL
///   ["p":protected]=>
///   string(1) "p"
///   ["q":"M":private]=>
///   float(2.5)
/// }
/// object(N)#2 (0) {
/// }
/// ```
///
/// A value that holds itself is php's recursion guard, not an
/// unbounded walk: `php_var_dump` marks the object it is inside and
/// prints `*RECURSION*` where the mark is already set, so the cycle
/// `ext/json`'s `004.phpt` opens with is one line and not a stack
/// overflow.
///
/// ```php
/// $a = new stdClass; $a->prop = $a; var_dump($a);
/// ```
/// ```expect
/// object(stdClass)#1 (1) {
///   ["prop"]=>
///   *RECURSION*
/// }
/// ```
///
/// The mark is cleared on the way out, so the second of two objects
/// reached side by side is printed in full and only a second visit down
/// one path is `*RECURSION*`; an array the walk passes through carries
/// the same guard on its own entry.
///
/// ```php
/// $a = new stdClass; $b = new stdClass; $b->v = 1; $a->prop = $a; $a->x = $b; $a->y = $b; var_dump($a);
/// $o = new stdClass; $o->a = [$o, 1]; var_dump($o);
/// ```
/// ```expect
/// object(stdClass)#1 (3) {
///   ["prop"]=>
///   *RECURSION*
///   ["x"]=>
///   object(stdClass)#2 (1) {
///     ["v"]=>
///     int(1)
///   }
///   ["y"]=>
///   object(stdClass)#2 (1) {
///     ["v"]=>
///     int(1)
///   }
/// }
/// object(stdClass)#3 (1) {
///   ["a"]=>
///   array(2) {
///     [0]=>
///     *RECURSION*
///     [1]=>
///     int(1)
///   }
/// }
/// ```
///
/// The array half of the guard is `GC_PROTECTED` on the array itself,
/// which a program reaches by binding an entry to the variable that
/// holds the array (`$a[] = &$a`): the entry is the cell the CV holds,
/// the cell holds the array, and the array is the one the walk is
/// already inside.
///
/// ```php
/// $a = []; $a[] = &$a; var_dump($a);
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   *RECURSION*
/// }
/// ```
///
/// A throwable is the same arm over the seven properties
/// `zend_register_default_exception` declares on `Exception` and
/// `Error`, the constructor's `message` and `code` in their slots, the
/// site `new` recorded in `file` and `line`, and `trace` the array of
/// frames below the top, none at the top level (design §5.9).
///
/// ```php
/// var_dump(new Exception('m', 3));
/// ```
/// ```expect
/// object(Exception)#1 (7) {
///   ["message":protected]=>
///   string(1) "m"
///   ["string":"Exception":private]=>
///   string(0) ""
///   ["code":protected]=>
///   int(3)
///   ["file":protected]=>
///   string(%d) "%s.php"
///   ["line":protected]=>
///   int(2)
///   ["trace":"Exception":private]=>
///   array(0) {
///   }
///   ["previous":"Exception":private]=>
///   NULL
/// }
/// ```
///
/// A property's name is printed through `zend_unmangle_property_name`
/// whatever table it came from, so a dynamic property carrying a
/// mangled name, which `(object)` over `(array)` of an object makes
/// (#294), is rendered with the visibility the prefix spells and
/// never as its bytes. A name that opens with `\0` and cannot be
/// unmangled is a notice where its row is, and then the bytes as they
/// are (#347).
///
/// ```php
/// class A { public $a = 1; protected $b = 2; private $c = 3; }
/// var_dump((object)(array)new A, (object)["\0one" => 4, "\0\0two" => 5, "\0B\0c\0d" => 6]);
/// ```
/// ```expect
/// object(stdClass)#1 (3) {
///   ["a"]=>
///   int(1)
///   ["b":protected]=>
///   int(2)
///   ["c":"A":private]=>
///   int(3)
/// }
/// object(stdClass)#2 (3) {
///
/// Notice: Corrupt member variable name in %s on line 3
///   ["%0one"]=>
///   int(4)
///
/// Notice: Illegal member variable name in %s on line 3
///   ["%0%0two"]=>
///   int(5)
///   ["d":"B":private]=>
///   int(6)
/// }
/// ```
///
/// A resource is `php_var_dump`'s `IS_RESOURCE` arm: its id and its
/// type's name, and `Unknown` once it is closed, since a closed
/// resource has no type; the stream `fopen` hands back is the witness,
/// with `ext/standard` linked.
///
/// ```php
/// $f = fopen('php://memory', 'r+'); var_dump($f); fclose($f); var_dump($f, [$f]);
/// ```
/// ```expect
/// resource(%d) of type (stream)
/// resource(%d) of type (Unknown)
/// array(1) {
///   [0]=>
///   resource(%d) of type (Unknown)
/// }
/// ```
///
/// The walk holds what php's holds (GH-19801): an array its own owner
/// across the rows, so an element's `__debugInfo` that frees the
/// array's last other owner leaves it standing, and an object's table
/// the owner `zend_get_properties_for` takes, `GC_TRY_ADDREF`, so a
/// nested `__debugInfo` that unsets a sibling separates the table and
/// this walk prints what it had, the next one what is left.
///
/// ```php
/// $a = [new class { function __debugInfo() { global $b; $b->a = null; gc_collect_cycles(); return []; } }];
/// $b = new stdClass; $b->a = &$a;
/// var_dump($b);
/// class D { function __debugInfo() { global $p; unset($p->x, $p->y); return ['d' => 1]; } }
/// $p = new stdClass; $p->x = new D; $p->y = str_repeat('y', 3);
/// var_dump($p, $p);
/// ```
/// ```expect
/// object(stdClass)#2 (1) {
///   ["a"]=>
///   &array(1) {
///     [0]=>
///     object(class@anonymous)#1 (0) {
///     }
///   }
/// }
/// object(stdClass)#1 (2) {
///   ["x"]=>
///   object(D)#3 (1) {
///     ["d"]=>
///     int(1)
///   }
///   ["y"]=>
///   string(3) "yyy"
/// }
/// object(stdClass)#1 (0) {
/// }
/// ```
///
/// `Z_PARAM_VARIADIC('+')` takes each argument as the call holds it, so
/// an `UNDEF` one is printed by `php_var_dump`'s `default:` arm: the
/// value of a call whose frame phpdbg left at `EG(exception_op)` with
/// nothing pending, which hands its caller nothing.
///
/// ```php
/// function f() {
///   $q = 1;
///   return 7;
/// }
/// var_dump(f());
/// ```
/// ```request phpdbg
/// register intdiv
/// b 3
/// r
/// intdiv 1
/// ev 9
/// c
/// q
/// ```
/// ```expect exit=255
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Registered intdiv]
/// prompt> [Breakpoint #0 added at %SCRATCH%/witness.php:3]
/// prompt> [Breakpoint #0 at %SCRATCH%/witness.php:3, hits: 1]
/// >00003:   $q = 1;
///  00004:   return 7;
///  00005: }
/// prompt>%s
/// prompt>%s
/// Fatal error: Uncaught ArgumentCountError: intdiv() expects exactly 2 arguments, 1 given in %SCRATCH%/witness.php:3
/// Stack trace:
/// #0 %SCRATCH%/witness.php(3): intdiv(1)
/// #1 %SCRATCH%/witness.php(6): f()
/// #2 {main}
///   thrown in %SCRATCH%/witness.php on line 3
/// prompt> UNKNOWN:0
/// [Script ended normally]
/// prompt>
/// ```
#[php_function]
fn var_dump(req: &mut Request, value: Value, values: Variadic<Value>) {
    for (at, coerced) in (1u32..).zip(std::iter::once(value).chain(values)) {
        let value = req
            .frames()
            .argument(at)
            .filter(|passed| passed.tag() == crate::api::values::TypeTag::Undef)
            .unwrap_or(coerced);
        dump(req, &value, 1);
    }
}

/// php's `php_printf("%*c", width, ' ')`: `width` blanks, written as
/// one piece; nothing for a width of zero, which is php's `level > 1`
/// test before it.
fn blanks(req: &mut Request, width: usize) {
    if width > 0 {
        req.response_mut().write(" ".repeat(width).as_bytes());
    }
}

/// One value as `php_var_dump(zv, level)` prints it, with its newline:
/// `level - 1` blanks before it past the first level, and an array's
/// elements each through `php_array_element_dump`, the key at `level + 1`
/// and the value at `level + 2`.
///
/// Each piece is written to the request as php's `php_printf`,
/// `PHPWRITE` and `PUTS` write it, one write per call of php's, so what
/// code the dump runs writes, a `__debugInfo` method, a destructor or a
/// handler, lands where php's lands, and an `ob_start` callback with a
/// chunk size of one is handed php's pieces:
///
/// ```php
/// class C { function __debugInfo() { echo "in debugInfo\n"; return []; } }
/// var_dump([new C, 1]);
/// ob_start(function ($chunk) { return $chunk === '' ? '' : "<$chunk>"; }, 1);
/// var_dump([1, "a"]);
/// ob_end_flush();
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   in debugInfo
/// object(C)#1 (0) {
///   }
///   [1]=>
///   int(1)
/// }
/// <array(2) {
/// ><  [0]=>
/// ><  ><int(1)
/// ><  [1]=>
/// ><  ><string(1) "><a><"
/// ><}
/// >
/// ```
///
/// The array and the object arms carry php's recursion guard: the walk
/// marks what it is inside with `Z_PROTECT_RECURSION_P` and clears the
/// mark on the way out, and a value already marked is `*RECURSION*` in
/// place of its contents, printed after the indent this level already
/// wrote and before the `&` a reference would have carried, since php
/// returns there ahead of `COMMON`. Without it a value that holds
/// itself is unbounded recursion, which is a panic reaching a request
/// (design §10).
fn dump(req: &mut Request, value: &Value, level: usize) {
    blanks(req, level - 1);
    // `COMMON`: php's `&` stands before the value a shared cell holds and
    // before nothing else, so a cell one name holds prints as the value
    // does; a cell is never in a cell, so this is read once.
    let common = match value.as_ref() {
        ValueRef::Reference(cell) if cell.heap().refcount() > 1 => "&",
        _ => "",
    };
    let value = &value.deref();
    if let Some(array) = value.as_array() {
        dump_array(req, common, array, level);
    } else if let Some(object) = value.as_object() {
        dump_object(req, common, object, level);
    } else {
        dump_scalar(req, common, value);
    }
}

/// [`dump`] over a value that holds no other, a call of its own, so its
/// texts are off the stack of a nest of objects or arrays dumped level
/// by level (#3159). A float is `%.*H` at `serialize_precision`, which
/// writes a non-finite one whole however short the precision:
///
/// ```php
/// ini_set('serialize_precision', 1);
/// debug_zval_dump(INF, -INF, NAN, 1.5);
/// ```
/// ```expect
/// float(INF)
/// float(-INF)
/// float(NAN)
/// float(2)
/// ```
#[inline(never)]
fn dump_scalar(req: &mut Request, common: &str, value: &Value) {
    let line = if let Some(b) = value.as_bool() {
        format!("{common}bool({b})\n")
    } else if let Some(n) = value.as_int() {
        format!("{common}int({n})\n")
    } else if let Some(f) = value.as_float() {
        // `php_var_dump` prints a float with `%.*H` at `serialize_precision`,
        // which writes a non-finite one whole, as `zend_gcvt` does at -1.
        let precision = if f.is_finite() {
            req.ini().serialize_precision()
        } else {
            -1
        };
        let text = php_heap::double_to_string(f, precision, php_heap::Exponent::Upper);
        format!("{common}float({text})\n")
    } else if let Some(s) = value.as_php_string() {
        req.response_mut()
            .write(format!("{common}string({}) \"", s.len()).as_bytes());
        req.response_mut().write(s.as_bytes());
        req.response_mut().write(b"\"\n");
        return;
    } else if let Some(resource) = value.as_resource() {
        // `IS_RESOURCE`: the list's id and the type's name through
        // `zend_rsrc_list_get_rsrc_type`, which is `Unknown` once
        // closed.
        let name = resource.type_name(req).map_or_else(
            || String::from("Unknown"),
            |name| String::from_utf8_lossy(name).into_owned(),
        );
        format!("{common}resource({}) of type ({name})\n", resource.id())
    } else if value.tag() == crate::api::values::TypeTag::Undef {
        // `php_var_dump`'s `default:` arm.
        format!("{common}UNKNOWN:0\n")
    } else {
        format!("{common}NULL\n")
    };
    req.response_mut().write(line.as_bytes());
}

/// `php_var_dump`'s `IS_ARRAY` arm: `GC_IS_RECURSIVE`, then
/// `GC_ADDREF(myht)` beside `GC_PROTECT_RECURSION(myht)`
/// (`Z/ext/standard/var.c:152`, GH-19801), which php takes only where the
/// array is refcounted: the walk holds an owner of its own, so an
/// element's `__debugInfo` that gives up the array's last other owner
/// leaves it standing until the walk is done, and a write through another
/// name separates rather than changing the rows under the walk. The
/// walk's owner is given up as `GC_DTOR_NO_REF(myht)` frees the array
/// where it was the last. The mark is asked before the header is
/// written, as php asks it, so a marked array writes `*RECURSION*` alone.
///
/// A call of its own, so the walk it keeps is off the stack of a nest of
/// objects dumped level by level, as [`dump_object`]'s is off an array's
/// (#3159).
#[inline(never)]
fn dump_array(req: &mut Request, common: &str, array: PhpArray, level: usize) {
    if array.is_recursive() {
        req.response_mut().write(b"*RECURSION*\n");
        return;
    }
    req.response_mut()
        .write(format!("{common}array({}) {{\n", array.len()).as_bytes());
    let held = Hold::Owner(GiveUp::Dtor);
    let walked = walk::foreach_guarded(req, array, held, |req, row| -> walk::Step {
        if too_deep(req, level, req.stack_limit().exhausted()) {
            return Ok(ControlFlow::Continue(()));
        }
        // `php_array_element_dump`.
        match row.key() {
            Key::Int(n) => req
                .response_mut()
                .write(format!("{:w$}[{n}]=>\n", "", w = level + 1).as_bytes()),
            Key::Str(s) => {
                req.response_mut()
                    .write(format!("{:w$}[\"", "", w = level + 1).as_bytes());
                req.response_mut().write(s.as_bytes());
                req.response_mut().write(b"\"]=>\n");
            }
        }
        dump(req, row.value(), level + 2);
        Ok(ControlFlow::Continue(()))
    });
    match walked {
        // Asked above, before anything was written; the body answers no
        // throwable.
        Ok(walk::Guarded::Recursive | walk::Guarded::Walked(_)) | Err(_) => {}
    }
    blanks(req, level - 1);
    req.response_mut().write(b"}\n");
}

/// `php_var_dump`'s `IS_OBJECT` arm: `object(Class)#handle (count) {`,
/// then each row of the table `zend_get_properties_for(struc,
/// ZEND_PROP_PURPOSE_DEBUG)` holds as `php_object_property_dump` prints
/// it: a class's own table where it shows one, which is what
/// `__debugInfo` and a `Closure` do, and every other class's members,
/// the declared ones in declaration order with the visibility php
/// mangles into the name, `["p":protected]` and `["q":"Class":private]`,
/// and a typed one nothing has written as `uninitialized(type)`, which
/// is in the listing and not in the count (`zend_array_count` over a
/// table with an empty `IS_INDIRECT`); then the dynamic ones.
///
/// php marks the object before it reads its properties and clears the
/// mark after the last one, so an object reached twice down one path is
/// `*RECURSION*` the second time and an object reached twice side by
/// side is printed in full both times.
///
/// The type an uninitialized property shows is `zend_type_to_string`'s,
/// the class names first, an intersection among them bracketed, then the
/// builtins in php's order:
///
/// ```php
/// class T { public int|string|null $x; public (Countable&Traversable)|array $y; }
/// var_dump(new T);
/// ```
/// ```expect
/// object(T)#1 (0) {
///   ["x"]=>
///   uninitialized(string|int|null)
///   ["y"]=>
///   uninitialized((Countable&Traversable)|array)
/// }
/// ```
///
/// ```php
/// class C { public int|string $a; public iterable $b; public null|int|bool $c; }
/// var_dump(new C);
/// ```
/// ```expect
/// object(C)#1 (0) {
///   ["a"]=>
///   uninitialized(string|int)
///   ["b"]=>
///   uninitialized(Traversable|array)
///   ["c"]=>
///   uninitialized(int|bool|null)
/// }
/// ```
///
/// A dynamic property is its own row whatever a parent declared private
/// under its name:
///
/// ```php
/// class P { private $c = 'declared'; }
/// class C extends P { function f() { $this->c = 'dynamic'; } }
/// $o = new C; @$o->f();
/// var_dump($o);
/// ```
/// ```expect
/// object(C)#1 (2) {
///   ["c":"P":private]=>
///   string(8) "declared"
///   ["c"]=>
///   string(7) "dynamic"
/// }
/// ```
///
/// A class's own table is read as any: a string key is unmangled as any
/// property name is, so a table that carries `\0*\0p` shows
/// `["p":protected]`, and a reference row is shown as `COMMON` shows
/// one, `&` before a value another name shares:
///
/// ```php
/// class D { function __debugInfo() { global $y; return ['a' => &$y, 'c' => 'z']; } }
/// $y = 5; var_dump(new D);
/// ```
/// ```expect
/// object(D)#1 (2) {
///   ["a"]=>
///   &int(5)
///   ["c"]=>
///   string(1) "z"
/// }
/// ```
///
/// The rows are lent as they stand, as php's `ZEND_HASH_FOREACH_KEY_VAL`
/// lends them, with no owner taken of any, so a count the program reads
/// while the dump runs is php's; and the header is written after the
/// table is asked for, so what a member's `__debugInfo` writes lands
/// after its indent and before its `object(`:
///
/// ```php
/// #[AllowDynamicProperties]
/// class O { public $n; }
/// class N { function __debugInfo() { global $o; debug_zval_dump($o->d, $o->n); return []; } }
/// $o = new O; $o->d = str_repeat('d', 3); $o->n = new N;
/// var_dump($o);
/// ```
/// ```expect
/// object(O)#1 (2) {
///   ["n"]=>
///   string(3) "ddd" refcount(2)
/// *RECURSION*
/// object(N)#2 (0) {
///   }
///   ["d"]=>
///   string(3) "ddd"
/// }
/// ```
///
/// ```php
/// #[AllowDynamicProperties]
/// class O { public $n; }
/// class N { function __debugInfo() { echo "inside\n"; return []; } }
/// $o = new O; $o->n = new N;
/// var_dump($o);
/// ```
/// ```expect
/// object(O)#1 (1) {
///   ["n"]=>
///   inside
/// object(N)#2 (0) {
///   }
/// }
/// ```
///
/// A call of its own, as [`dump_array`] is.
#[inline(never)]
fn dump_object(req: &mut Request, common: &str, object: PhpObject, level: usize) {
    // `php_var_dump`'s `ZEND_ACC_ENUM` arm, ahead of the recursion
    // guard as php has it.
    if req.class_is(object.class(), ClassKind::Enum) {
        dump_enum(req, common, object);
        return;
    }
    if object.is_recursive_in(req, RecursionGuard::Debug) {
        req.response_mut().write(b"*RECURSION*\n");
        return;
    }
    object.protect_in(req, RecursionGuard::Debug);
    let held = match object.properties_for(req, PropertyPurpose::Debug) {
        Ok(held) => held,
        Err(thrown) => {
            object.unprotect_in(req, RecursionGuard::Debug);
            req.leave_pending(&thrown);
            return;
        }
    };
    dump_object_header(req, common, object, held.count());
    // `ZEND_HASH_FOREACH_KEY_VAL(myht, num, key, val)`
    // (`Z/ext/standard/var.c:196`) over the table held until
    // `zend_release_properties`: the walk takes no owner of its own.
    // What a row writes before its value is a call of its own, so a
    // nest of objects keeps at each level only the walk and the table
    // (#3159).
    let walked = walk::properties(req, &held, Unwritten::Shown, |req, row| -> walk::Step {
        if dump_property_head(req, level, row) {
            dump(req, row.value(), level + 2);
        }
        Ok(ControlFlow::Continue(()))
    });
    match walked {
        // The body answers no throwable.
        Ok(_) | Err(_) => {}
    }
    // `zend_release_properties`, then the closing brace, then
    // `ZEND_GUARD_OR_GC_UNPROTECT_RECURSION`, php's order, so what the
    // release's destructor writes lands before the brace and finds the
    // object still marked.
    let left = held.release(req);
    blanks(req, level - 1);
    req.response_mut().write(b"}\n");
    object.unprotect_in(req, RecursionGuard::Debug);
    if let Err(thrown) = left {
        leave_chained(req, thrown);
    }
}

/// `php_var_dump`'s `ZEND_ACC_ENUM` arm: a case is its class and its
/// case name, `zend_enum_fetch_case_name`, and none of its properties.
#[inline(never)]
fn dump_enum(req: &mut Request, common: &str, object: PhpObject) {
    let class = req.symbols().class_name(object.class());
    let case = object
        .prop(req, b"name")
        .and_then(|name| name.as_str_bytes().map(<[u8]>::to_vec))
        .unwrap_or_default();
    req.response_mut().write(
        format!(
            "{common}enum({class}::{})\n",
            String::from_utf8_lossy(&case)
        )
        .as_bytes(),
    );
}

/// `Z_OBJ_HANDLER_P(struc, get_class_name)`, then
/// `php_var_dump_object_prefix`, read after the table as php reads
/// them: what a lazy object is, before `object(`, and the `count` rows
/// the table holds. The name as its bytes stand, as php's `%s` prints
/// `ZSTR_VAL`.
#[inline(never)]
fn dump_object_header(req: &mut Request, common: &str, object: PhpObject, count: usize) {
    let name = object.handlers(req).get_class_name(req, object);
    let prefix = match object.laziness() {
        Laziness::Eager => "",
        Laziness::Ghost => "lazy ghost ",
        Laziness::Proxy | Laziness::Forwarding => "lazy proxy ",
    };
    let mut header = format!("{common}{prefix}object(").into_bytes();
    header.extend_from_slice(php_unit::decl::shown_bytes(&name));
    header.extend_from_slice(format!(")#{} ({count}) {{\n", object.index()).as_bytes());
    req.response_mut().write(&header);
}

/// `php_object_property_dump` up to its value: the refusal where the
/// stack has come down to the limit ([`too_deep`]), else the row's key,
/// and for a typed property nothing has written its `uninitialized(T)`;
/// whether the value is still to be dumped.
#[inline(never)]
fn dump_property_head(req: &mut Request, level: usize, row: &walk::Row) -> bool {
    if too_deep(req, level, req.stack_limit().overflowed()) {
        return false;
    }
    match row.key() {
        Key::Int(at) => req
            .response_mut()
            .write(format!("{:w$}[{at}]=>\n", "", w = level + 1).as_bytes()),
        Key::Str(name) => dump_property_name(req, level, name.as_bytes()),
    }
    match row.uninitialized() {
        Some(ty) => {
            let line = format!("{:w$}uninitialized({ty})\n", "", w = level + 1);
            req.response_mut().write(line.as_bytes());
            false
        }
        None => true,
    }
}

/// What a class's debug table raised, left in `EG(exception)` once its
/// rows are printed, as `php_var_dump` leaves it
/// (`Z/ext/standard/var.c:184`): chained onto a throwable an earlier
/// object of the same dump left there, as `zend_throw_exception_internal`
/// chains it (`Thrown::chain`).
///
/// ```php
/// $n = (new ReflectionClass('DOMNode'))->newInstanceWithoutConstructor();
/// foreach ([[$n], [$n, $n]] as $a) {
///     try { var_dump($a); } catch (DOMException $e) { for ($i = 0; $e; $e = $e->getPrevious()) $i++; echo $i, "\n"; }
/// }
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   object(DOMNode)#2 (0) {
///   }
/// }
/// 18
/// array(2) {
///   [0]=>
///   object(DOMNode)#2 (0) {
///   }
///   [1]=>
///   object(DOMNode)#2 (0) {
///   }
/// }
/// 36
/// ```
#[cold]
#[inline(never)]
fn leave_chained(req: &mut Request, thrown: crate::api::raising::Thrown) {
    use crate::api::request::Raised;
    let thrown = match req.take_raised() {
        Some(Raised::Throwable(earlier)) => {
            crate::api::raising::Thrown::of(Some(earlier)).chain(req, thrown)
        }
        other => {
            req.set_raised(other);
            thrown
        }
    };
    req.leave_pending(&thrown);
}

/// `php_object_property_dump`'s `ZEND_CHECK_STACK_LIMIT` arm
/// (`Z/ext/standard/var.c:59`): where the stack has passed
/// `zend.max_allowed_stack_size`, the property is `nesting level too
/// deep` after its indent, with no newline, in place of its name and
/// value, so a chain deeper than the stack ends there and not in a
/// crash.
///
/// gh20840's shape under the small-stack regime, where 256K is passed
/// well inside the chain in both engines: the dump goes to an
/// `ob_start` callback that answers only whether the refusal was in it, so the
/// depth each engine reached, which its own frames decide, is not what
/// is pinned. The chain is taken apart a link at a time after, so its
/// release is no deeper than the stack either.
///
/// An array's element is the same refusal at the thread's floor rather
/// than its limit: php's `php_array_element_dump` asks nothing and runs
/// to the thread's end, so an array nested past the limit under a small
/// `zend.max_allowed_stack_size` is dumped whole in both engines
/// (`php_compile::StackLimit::exhausted`).
///
/// ```php
/// $o = $first = new stdClass;
/// for ($i = 0; $i < 50000; $i++) { $o->next = new stdClass; $o = $o->next; }
/// ob_start(fn ($chunk) => str_contains($chunk, 'nesting level too deep') ? "nesting level too deep\n" : "the whole chain\n");
/// var_dump($first);
/// ob_end_flush();
/// while ($next = $first->next ?? null) { $first->next = $next->next ?? null; }
/// ```
/// ```expect regime=small-stack
/// nesting level too deep
/// ```
fn too_deep(req: &mut Request, level: usize, past: bool) -> bool {
    if !past {
        return false;
    }
    req.response_mut()
        .write(format!("{:w$}nesting level too deep", "", w = level + 1).as_bytes());
    true
}

/// `php_object_property_dump`'s string key: the name between `[` and
/// `]=>` as `zend_unmangle_property_name` reads it, `"p":protected` for
/// a class part of `*`, `"p":"Class":private` for any other, and `"p"`
/// for a name with no class part; the property and the class are
/// printed with `%s`, so each stops at a `\0` of its own. A name php
/// cannot unmangle is its bytes in quotes, after the notice
/// `zend_unmangle_property_name_ex` raises for it, which php raises
/// before the row's indent.
fn dump_property_name(req: &mut Request, level: usize, name: &[u8]) {
    let unmangled = unmangle_property_name(name);
    if let Unmangled::Illegal(notice) = unmangled {
        crate::api::raising::notice(req, notice);
    }
    req.response_mut()
        .write(format!("{:w$}[", "", w = level + 1).as_bytes());
    match unmangled {
        Unmangled::Mangled { class, prop } => {
            let mut shown = b"\"".to_vec();
            shown.extend_from_slice(c_str(prop));
            if class.first() == Some(&b'*') {
                shown.extend_from_slice(b"\":protected");
            } else {
                shown.extend_from_slice(b"\":\"");
                shown.extend_from_slice(c_str(class));
                shown.extend_from_slice(b"\":private");
            }
            req.response_mut().write(&shown);
        }
        Unmangled::Plain | Unmangled::Illegal(_) => {
            req.response_mut().write(b"\"");
            req.response_mut().write(name);
            req.response_mut().write(b"\"");
        }
    }
    req.response_mut().write(b"]=>\n");
}

/// What `zend_unmangle_property_name_ex` answers for one name.
#[derive(Clone, Copy)]
pub(crate) enum Unmangled<'a> {
    /// A name with no `\0` at its head: `SUCCESS` with no class.
    Plain,
    /// The class part and the property: `SUCCESS` with a class.
    Mangled { class: &'a [u8], prop: &'a [u8] },
    /// `FAILURE`, with the notice php raises for it.
    Illegal(&'static str),
}

/// `zend_unmangle_property_name_ex` (`Z/Zend/zend_compile.c:1575`): a
/// name opening with `\0` is split at its second `\0` into the class
/// part and the property, and where the property holds a `\0` of its
/// own, an anonymous class's source suffix, the class part runs through
/// it. The notice is `Illegal member variable name` for a name shorter
/// than three bytes or with `\0` at its second byte, and `Corrupt
/// member variable name` for one with no second `\0`.
pub(crate) fn unmangle_property_name(name: &[u8]) -> Unmangled<'_> {
    let Some(rest) = name.strip_prefix(b"\0") else {
        return Unmangled::Plain;
    };
    if rest.len() < 2 || rest.first() == Some(&0) {
        return Unmangled::Illegal("Illegal member variable name");
    }
    // `zend_strnlen(ZSTR_VAL(name) + 1, ZSTR_LEN(name) - 2)`: the second
    // `\0` is looked for short of the last byte, so the property is at
    // least one byte.
    let Some(at) = rest.iter().take(rest.len() - 1).position(|&b| b == 0) else {
        return Unmangled::Illegal("Corrupt member variable name");
    };
    let (class, prop) = (&rest[..at], &rest[at + 1..]);
    match prop.iter().position(|&b| b == 0) {
        Some(inner) => Unmangled::Mangled {
            class: &rest[..at + 1 + inner],
            prop: &prop[inner + 1..],
        },
        None => Unmangled::Mangled { class, prop },
    }
}

/// `memory_get_usage(bool $real_usage = false): int`, php's
/// `zend_memory_usage`: the bin accounting the request has out, or under
/// `$real_usage` the bytes its chunks hold.
///
/// A fresh request holds php's one 2 MiB chunk and nothing more, so the
/// second number is that chunk and the oracle says so. The first is each
/// engine's own bookkeeping and is not pinned here: what php's request
/// heap holds beside the interned strings is on the system allocator
/// in this engine, the op array, the compiler and the tree, which
/// design §5.13 states as the one approximation and the divergence row
/// on the witness of `php_heap::string::Table` records. What both answer
/// of the first is how `$real_usage` is read, `ZEND_PARSE_PARAMETERS_START(0, 1)`
/// with `Z_PARAM_BOOL`: optional and `false` without it, and a scalar
/// coerced as a bool parameter of a native is, 1 to `true`.
///
/// ```php
/// var_dump(memory_get_usage(true), memory_get_peak_usage(true));
/// memory_reset_peak_usage();
/// var_dump(memory_get_peak_usage(true));
/// var_dump(memory_get_usage() === memory_get_usage(false), memory_get_usage(1) === memory_get_usage(true));
/// ```
/// ```expect
/// int(2097152)
/// int(2097152)
/// int(2097152)
/// bool(true)
/// bool(true)
/// ```
///
/// What one allocation moves both numbers by is a number both engines
/// answer, since every string a native builds is the request's heap's
/// (design §5.13, #174, #211). A string past 2 MiB is a huge block,
/// rounded to the page the machine reports, sixteen KiB where the
/// oracle is pinned, and `zend_mm_add_huge_block` takes the block's list
/// node from the 24-byte bin of the same heap before the block is
/// counted, so the bin accounting moves by the rounded block and the
/// node, and the chunks' by the block alone:
///
/// ```php
/// $b = memory_get_usage(); $rb = memory_get_usage(true); $s = str_repeat("x", 3000000); var_dump(memory_get_usage() - $b, memory_get_usage(true) - $rb);
/// ```
/// ```expect
/// int(3014680)
/// int(3014656)
/// ```
#[php_function]
fn memory_get_usage(req: &mut Request, #[php(default = false)] real_usage: bool) -> i64 {
    as_long(req.memory().usage(real_usage))
}

/// `memory_get_peak_usage(bool $real_usage = false): int`, php's
/// `zend_memory_peak_usage`: the high-water mark of the same two
/// numbers since the request began or the peak was last reset.
///
/// A packed array's growth is where the peak reads an arm of
/// `zend_mm_realloc_heap` that the usage never sees, so the witness
/// resets the peak and takes both numbers as a delta: the base each
/// engine starts from differs by design §5.13's approximation, and what
/// one growth moves does not (#174, #278).
///
/// A packed table of `n` slots is `HT_HASH_SIZE(HT_MIN_MASK)` beside
/// `n` zvals, 8 + 16n, and the 56-byte `zend_array` is an allocation of
/// its own. `zend_hash_packed_grow` doubles, so no doubling of a packed
/// table ever lands in the bin it came from and the same-bin arm of
/// `Mm::realloc` is never the one a growing table takes; what it takes
/// below 3072 bytes is `zend_mm_realloc_slow`
/// (`Z/Zend/zend_alloc.c:1569`), which holds the new slot while the old
/// one still stands and then puts the peak back at
/// `MAX(orig_peak, heap->size)`. Sixty-five appends end on 128 slots,
/// 2056 bytes, the 2560 bin, and every growth under them is that arm:
/// the peak reads 2560 + 56 and not the 1280-byte slot that stood
/// beside the last of them.
///
/// Past 256 slots a packed table leaves the bins. Three hundred appends
/// end on 512 slots, 8200 bytes, a `run` of three pages the two-page
/// table grew into where it stood, so 12288 + 56. A 5000-byte string
/// laid down after it takes the two pages that follow, and the growth
/// to 1024 slots cannot stay: it moves, and the peak is again what the
/// slow arm restores, 20480 + 8192 + 56, never the old `run` beside the
/// new one (#278).
///
/// ```php
/// $bin = []; memory_reset_peak_usage(); $b = memory_get_usage();
/// for ($i = 0; $i < 65; $i++) { $bin[] = $i; }
/// var_dump(memory_get_usage() - $b, memory_get_peak_usage() - $b);
/// $pages = []; memory_reset_peak_usage(); $b = memory_get_usage();
/// for ($i = 0; $i < 300; $i++) { $pages[] = $i; }
/// var_dump(memory_get_usage() - $b, memory_get_peak_usage() - $b);
/// $after = str_repeat("x", 5000);
/// for ($i = 300; $i < 900; $i++) { $pages[] = $i; }
/// var_dump(memory_get_usage() - $b, memory_get_peak_usage() - $b);
/// ```
/// ```expect
/// int(2616)
/// int(2616)
/// int(12344)
/// int(12344)
/// int(28728)
/// int(28728)
/// ```
#[php_function]
fn memory_get_peak_usage(req: &mut Request, #[php(default = false)] real_usage: bool) -> i64 {
    as_long(req.memory().peak(real_usage))
}

/// `memory_reset_peak_usage(): void`, php's
/// `zend_memory_reset_peak_usage`: both peaks become what is in use now.
/// `ZEND_PARSE_PARAMETERS_NONE` takes no argument at all, so one is
/// php's `ArgumentCountError` and not an ignored extra.
///
/// ```php
/// $s = str_repeat("x", 4000000); unset($s);
/// $before = memory_get_peak_usage();
/// memory_reset_peak_usage();
/// var_dump(memory_get_peak_usage() < $before - 3000000);
/// try { memory_reset_peak_usage(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// memory_reset_peak_usage() expects exactly 0 arguments, 1 given
/// ```
#[php_function]
fn memory_reset_peak_usage(req: &mut Request) {
    req.reset_memory_peak();
}

/// A byte count as php's `RETURN_LONG` returns one.
fn as_long(bytes: usize) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

/// `extension_loaded(string $extension): bool`, php's
/// `zend_hash_exists` over the extension table, case-insensitively.
///
/// ```php
/// var_dump(extension_loaded('Core'), extension_loaded('CORE'), extension_loaded('nope'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
///
/// A diagnostic raised while a native's body runs stands where the call
/// does, since `zend_get_executed_lineno` answers off the caller's
/// opline and a native pushes no opline of its own.
///
/// ```php
/// var_dump(extension_loaded(NAN));
/// ```
/// ```expect
/// Warning: unexpected NAN value was coerced to string in %s on line 2
/// bool(false)
/// ```
#[php_function]
fn extension_loaded(req: &mut Request, extension: PhpString) -> bool {
    let lowered = extension.as_bytes().to_ascii_lowercase();
    req.extensions()
        .loaded_extensions()
        .iter()
        .any(|name| name.as_bytes().to_ascii_lowercase() == lowered)
}

/// `ini_get(string $option): string|false`, php's `PHP_FUNCTION(ini_get)`
/// (`Z/ext/standard/basic_functions.c:1894`): the entry's current bytes
/// as a string of the request's, `zend_ini_get_value`, or `false` for a
/// name the table has no entry under. The option is coerced as a string
/// parameter of a native is, `null` being the 8.1 deprecation and then
/// no entry.
///
/// ```php
/// var_dump(ini_get('precision'), ini_get('nope'), ini_set('precision', '10'), ini_get('precision'), ini_get(null));
/// ```
/// ```expect
/// Deprecated: ini_get(): Passing null to parameter #1 ($option) of type string is deprecated in %s on line 2
/// string(2) "14"
/// bool(false)
/// string(2) "14"
/// string(2) "10"
/// bool(false)
/// ```
///
/// The value is handed back as `ZVAL_SET_INI_STR` hands it: the
/// string the entry holds, the interned master value or the string a
/// changed entry took, so a read allocates nothing:
///
/// ```php
/// $m = memory_get_usage(); $x = ini_get('memory_limit'); echo memory_get_usage() - $m, "\n";
/// $tz = 'Europe/' . str_repeat('Amsterdam', mt_rand(1, 1)); ini_set('date.timezone', $tz); unset($tz);
/// $m = memory_get_usage(); $y = ini_get('date.timezone'); echo memory_get_usage() - $m, " $y\n";
/// ```
/// ```expect
/// 0
/// 0 Europe/Amsterdam
/// ```
///
/// A value the host set for an entry whose `on_modify` takes it as the
/// engine starts is interned with the rest, as
/// `zend_register_ini_entries_ex` interns it:
///
/// ```php
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'date.timezone=Europe/Amsterdam', '-r', '$m = memory_get_usage(); $x = ini_get("date.timezone"); echo memory_get_usage() - $m, " $x\n";'], [1 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]); proc_close($p);
/// ```
/// ```expect process
/// 0 Europe/Amsterdam
/// ```
#[php_function]
fn ini_get(req: &mut Request, option: PhpString) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    Ok(match req.ini_string(option.as_bytes())? {
        Some(value) => OrFalse::Value(value),
        None => OrFalse::False,
    })
}

/// `string|int|float|bool|null`, the value `ini_set` declares and reads
/// raw, refusing what is none of these before it reads the table.
type IniValue = Option<Union<PhpString, Union<i64, Union<f64, bool>>>>;

/// `ini_set(string $option, string|int|float|bool|null $value):
/// string|false`, php's `PHP_FUNCTION(ini_set)`: the entry's value
/// before the call, or `false` for a name the table has no entry under,
/// and the new value as `zval_get_string` reads the argument, so `10`
/// and `'10'` set the same bytes, `true` is `1` and `null` is empty. An
/// array, an object or a resource is php's `TypeError` before the table
/// is read. What a set entry moves is read where the entry is: here
/// `precision`, at the next float echoed.
///
/// ```php
/// var_dump(ini_set('precision', '10'), ini_set('precision', 10), ini_set('nope', '1'));
/// echo 1/3;
/// ```
/// ```expect
/// string(2) "14"
/// string(2) "10"
/// bool(false)
/// 0.3333333333
/// ```
///
/// `ZEND_PARSE_PARAMETERS_START(2, 2)`: fewer and more are one
/// `ArgumentCountError`, and the option is coerced as a string parameter
/// of a native is, `null` being the 8.1 deprecation and then no entry.
///
/// ```php
/// var_dump(ini_set('precision', null), ini_set('precision', true), ini_set('precision', 1.5));
/// ```
/// ```expect
/// string(2) "14"
/// string(0) ""
/// string(1) "1"
/// ```
///
/// `ini_alter`, the stub's `@alias ini_set`, is the same entry under a
/// second name with `ini_set`'s declaration:
///
/// ```php
/// var_dump(ini_alter('precision', 10), ini_get('precision'), ini_alter('nope', 1));
/// ```
/// ```expect
/// string(2) "14"
/// string(2) "10"
/// bool(false)
/// ```
#[php_function(aliases(ini_alter))]
fn ini_set(
    req: &mut Request,
    option: PhpString,
    value: Raw<IniValue>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let value = &value.value();
    if matches!(
        value.as_ref(),
        ValueRef::Array(_) | ValueRef::Object(_) | ValueRef::Resource(_)
    ) {
        return Err(arg_type_error(
            req,
            2,
            "be of type string|int|float|bool|null",
        ));
    }
    let value = to_string(req, value)?;
    // "open basedir check": four entries that name a path are held to
    // the guard before they are set.
    if matches!(
        option.as_bytes(),
        b"java.class.path" | b"java.home" | b"java.library.path" | b"vpopmail.directory"
    ) && req
        .environment_mut()
        .check_open_basedir(value.as_bytes(), true)
        .is_err()
    {
        value.give_up(req);
        return Ok(OrFalse::False);
    }
    let set = req.ini_mut().set_string(option.as_bytes(), *value);
    value.give_up(req);
    Ok(match set? {
        Some(previous) => OrFalse::Value(PhpString::new(req, &previous)?),
        None => OrFalse::False,
    })
}

/// `set_time_limit(int $seconds): bool`, php's `PHP_FUNCTION(set_time_limit)`
/// (`Z/main/main.c:1631`): `max_execution_time` moved to the seconds at
/// runtime, which unsets the execution timer and arms it again from
/// here, and `true` for the entry having taken the value. The seconds
/// are read as php's `"l"` reads them, so `'5'` and `5.0` are `5`, and
/// `null` is the 8.1 deprecation and then zero, which arms no timer.
///
/// ```php
/// var_dump(set_time_limit(5), ini_set('max_execution_time', '0'), set_time_limit('7'), ini_set('max_execution_time', 0));
/// ```
/// ```expect
/// bool(true)
/// string(1) "5"
/// bool(true)
/// string(1) "7"
/// ```
///
/// What the seconds do once armed is the witness beside
/// [`crate::interrupt::interrupted`].
#[php_function]
fn set_time_limit(req: &mut Request, seconds: i64) -> bool {
    req.set_time_limit(seconds)
}

/// `error_reporting(?int $error_level = null): int`, php's
/// `ZEND_FUNCTION(error_reporting)` (`Z/Zend/zend_builtin_functions.c:457`):
/// `EG(error_reporting)` as it stood, and, for a level given that is
/// not `null` and not the one standing, the level written into the
/// word and into the ini entry's value ([`Request::alter_error_reporting`]),
/// past the entry's `on_modify`. The level is read as `Z_PARAM_LONG_OR_NULL`
/// reads it; `null` asks and writes nothing.
///
/// ```php
/// var_dump(error_reporting(), error_reporting(30717), error_reporting(null), ini_get("error_reporting"), error_reporting("8"), ini_get("error_reporting"));
/// echo $u;
/// var_dump(error_reporting(32767));
/// echo $u;
/// ```
/// ```expect
/// int(30719)
/// int(30719)
/// int(30717)
/// string(5) "30717"
/// int(30717)
/// string(1) "8"
/// int(8)
///
/// Warning: Undefined variable $u in %s on line 5
/// ```
///
/// `ini_set` moves the same word through `OnUpdateErrorReporting`,
/// `atoi` of the text:
///
/// ```php
/// var_dump(ini_set("error_reporting", "2x"), error_reporting(), ini_set("error_reporting", "E_ALL"), error_reporting());
/// ```
/// ```expect
/// string(5) "30719"
/// int(2)
/// string(2) "2x"
/// int(0)
/// ```
#[php_function]
fn error_reporting(req: &mut Request, #[php(default = null)] error_level: Option<i64>) -> i64 {
    let old = req.diagnostics().error_reporting();
    if let Some(level) = error_level
        && level != old
    {
        req.alter_error_reporting(level);
    }
    old
}

/// `register_shutdown_function(callable $callback, mixed ...$args): void`,
/// php's `PHP_FUNCTION(register_shutdown_function)`
/// (`Z/ext/standard/basic_functions.c:1738`) over
/// `append_user_shutdown_function`: the callback resolved here, where
/// php's `"F*"` resolves it, and not where it is served, so a name no
/// function table has raises at the registration; the entry is one more
/// owner of the callback and of every extra argument, `zend_fcc_addref`
/// and the `ZVAL_COPY` loop over `entry.params`. The entries are the
/// request's from here and [`call_shutdown_functions`] serves them at
/// `php_request_shutdown`'s first step (design §5.10).
///
/// The registration order is the serving order, and each entry's extra
/// arguments are handed to its callback as they were passed:
///
/// ```php
/// function one() { echo "one\n"; }
/// function two($x, $y) { echo "two ", $x, $y, "\n"; }
/// register_shutdown_function('one');
/// register_shutdown_function('two', 5, "!");
/// echo "end\n";
/// ```
/// ```expect
/// end
/// one
/// two 5!
/// ```
///
/// The walk stands before the first destructor of the shutdown walk, so
/// a shutdown function is served while a global still holds its object
/// and its destructor is served after:
///
/// ```php
/// class A { function __destruct() { echo "d\n"; } }
/// function bye() { echo "bye\n"; }
/// register_shutdown_function('bye');
/// $a = new A;
/// echo "end\n";
/// ```
/// ```expect
/// end
/// bye
/// d
/// ```
///
/// The entry owns what its callable resolved to, `zend_fcc_addref`: a
/// `Class::method` string registered with `$this` in scope names the
/// object the call is bound to, which the entry keeps past the
/// expression that made it, so the walk serves the method on it and its
/// destructor comes after:
///
/// ```php
/// class Test {
///     function register() { register_shutdown_function('Test::method'); }
///     function method() { var_dump($this); }
///     function __destruct() { echo "destructed\n"; }
/// }
/// (new Test)->register();
/// $a = [1, 2, 3];
/// echo "end\n";
/// ```
/// ```expect
/// end
/// object(Test)#1 (0) {
/// }
/// destructed
/// ```
///
/// A callback that names nothing is php's `TypeError` at the
/// registration, worded as `zend_parse_arg_func` words the two misses,
/// and no argument at all is php's `ArgumentCountError`:
///
/// ```php
/// register_shutdown_function('nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: register_shutdown_function(): Argument #1 ($callback) must be a valid callback, function "nope" not found or invalid function name in %s:2
/// Stack trace:
/// #0 %s(2): register_shutdown_function('nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// register_shutdown_function();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: register_shutdown_function() expects at least 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): register_shutdown_function()
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn register_shutdown_function(
    req: &mut Request,
    callback: Callable,
    args: Variadic<Value>,
) -> Result<(), Thrown> {
    let callback = Owner::take(req, callback);
    for value in args.iter() {
        req.addref(value);
    }
    let arguments = args.into_iter().collect();
    req.code_mut()
        .register_shutdown_function(callback, arguments)
}

/// `php_call_shutdown_functions` (`Z/ext/standard/basic_functions.c:1650`),
/// step 1 of `php_request_shutdown` and so the first thing the request
/// does once the loop has returned (design §5.10): `zend_hash_apply`
/// over the entries `register_shutdown_function` appended, each served
/// through `zend_call_known_fcc` in the order they were appended. The
/// walk reads the list's length again on every turn, as php's reads
/// `nNumUsed`, so an entry a shutdown function appends is served by the
/// same walk.
///
/// The main op array's variables are the request's here, the symbol
/// table [`Request::detach_symbol_table`] left behind, and
/// [`Request::shutdown_destructors`] has not walked it yet. So a
/// shutdown function reads what the top level wrote and
/// `memory_get_usage()` still counts what those variables own, which is
/// the half of #280 that waited on this one: a three-megabyte string a
/// top-level variable holds is still out at the walk, where a request
/// that had released that variable with the frame would read the
/// baseline back.
///
/// ```php
/// function show($b) { var_dump(memory_get_usage() - $b > 2900000); }
/// $b = memory_get_usage();
/// $s = str_repeat("x", 3000000);
/// register_shutdown_function('show', $b);
/// echo "end\n";
/// ```
/// ```expect
/// end
/// bool(true)
/// ```
///
/// What one leaves pending ends the walk, as the `zend_bailout`
/// `zend_exception_error` raises leaves the `zend_try` php wraps the
/// walk in: the throwable is rendered as the uncaught fatal at the
/// boundary `zend_trace_to_string` names `[internal function]`, and the
/// entries behind it are not served.
///
/// ```php
/// function boom() { echo "boom\n"; throw new Exception("x"); }
/// function after() { echo "after\n"; }
/// register_shutdown_function('boom');
/// register_shutdown_function('after');
/// echo "end\n";
/// ```
/// ```expect exit=255
/// end
/// boom
///
/// Fatal error: Uncaught Exception: x in %s:2
/// Stack trace:
/// #0 [internal function]: boom()
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn call_shutdown_functions(req: &mut Request) {
    let mut at = 0;
    while let Some((position, callback, arguments)) = req.shutdown_function(at) {
        at = position + 1;
        match crate::api::call::call(req, &callback, &arguments) {
            Ok(value) => value.give_up(req),
            Err(thrown) => {
                req.leave_pending(&thrown);
                req.uncaught().ok();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::Engine;
    use crate::test::{Capture, STARTED};

    /// What the engine wrote and what ended it, for one source.
    fn served(source: &str) -> (String, Option<String>) {
        let served = Engine::builder()
            .build()
            .serve_capture(source, Capture::default());
        (served.stdout_text(), served.fatal)
    }

    /// The numbers `source` dumps, one `int(n)` a line, in the order it
    /// dumps them.
    fn dumped(source: &str) -> Vec<usize> {
        let (body, fatal) = served(source);
        assert_eq!(fatal, None, "{source}");
        body.lines()
            .map(|line| {
                line.strip_prefix("int(")
                    .and_then(|n| n.strip_suffix(')'))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or_else(|| panic!("{source} dumped {line:?}, not an int"))
            })
            .collect()
    }

    /// The bin accounting a request reads stands above what it had out
    /// before its first line ([`STARTED`]), since compiling and serving
    /// one lays more down, and below the bytes its chunks hold, since
    /// every bin is in a chunk. Both read in one request: the numbers
    /// themselves are each engine's own (design §5.13), and the witness
    /// on `memory_get_usage` holds what php and this engine both answer,
    /// how `$real_usage` is read.
    #[test]
    fn the_usage_stands_above_the_start_and_below_the_chunks() {
        let &[usage, real] =
            dumped("<?php var_dump(memory_get_usage(), memory_get_usage(true));").as_slice()
        else {
            panic!("two numbers dumped");
        };
        assert!(usage > STARTED, "{usage} is past the start, {STARTED}");
        assert!(usage < real, "{usage} is within the chunks' {real}");
    }

    /// `zend_wrong_parameters_count_error` for a call outside the arity,
    /// held here without the oracle; the trace such a call prints, with
    /// its arguments, is the witness beside `Request::trace_arguments`.
    /// php's own text, from the pinned oracle:
    ///
    /// ```text
    /// ArgumentCountError: memory_get_usage() expects at most 1 argument, 2 given
    /// ArgumentCountError: memory_get_peak_usage() expects at most 1 argument, 2 given
    /// ArgumentCountError: memory_reset_peak_usage() expects exactly 0 arguments, 1 given
    /// ArgumentCountError: ini_set() expects exactly 2 arguments, 1 given
    /// ArgumentCountError: ini_set() expects exactly 2 arguments, 3 given
    /// ArgumentCountError: set_time_limit() expects exactly 1 argument, 0 given
    /// ```
    #[test]
    fn a_call_past_the_maximum_is_php_s_argument_count_error() {
        for (source, text) in [
            (
                "<?php ini_set('precision');",
                "ini_set() expects exactly 2 arguments, 1 given",
            ),
            (
                "<?php ini_set('precision', 1, 2);",
                "ini_set() expects exactly 2 arguments, 3 given",
            ),
            (
                "<?php set_time_limit();",
                "set_time_limit() expects exactly 1 argument, 0 given",
            ),
            (
                "<?php memory_get_usage(true, true);",
                "memory_get_usage() expects at most 1 argument, 2 given",
            ),
            (
                "<?php memory_get_peak_usage(true, true);",
                "memory_get_peak_usage() expects at most 1 argument, 2 given",
            ),
            (
                "<?php memory_reset_peak_usage(true);",
                "memory_reset_peak_usage() expects exactly 0 arguments, 1 given",
            ),
        ] {
            let (_, fatal) = served(source);
            assert_eq!(
                fatal.as_deref().map(|f| f.contains(text)),
                Some(true),
                "{source} came to {fatal:?}"
            );
        }
    }

    /// `memory_reset_peak_usage()` leaves both peaks at what the request
    /// has out (`zend_memory_reset_peak_usage`): each usage read after
    /// it, before its own peak, is at most that peak, and with no chunk
    /// mapped since, the chunks' peak is the chunks' usage. The witness
    /// beside `memory_reset_peak_usage` holds against the oracle that a
    /// reset lowers the peak; this holds where it leaves it, with no
    /// oracle on the machine.
    #[test]
    fn a_reset_leaves_the_peak_at_what_the_request_has_out() {
        let &[usage, peak, real, real_peak] = dumped(
            "<?php memory_reset_peak_usage(); var_dump(memory_get_usage(), memory_get_peak_usage(), memory_get_usage(true), memory_get_peak_usage(true));",
        )
        .as_slice() else {
            panic!("four numbers dumped");
        };
        assert!(
            peak >= usage,
            "the peak {peak} is at least the usage {usage}"
        );
        assert_eq!(real_peak, real, "the chunks' peak is the chunks' usage");
    }
}
