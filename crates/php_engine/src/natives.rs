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

use php_unit::decl::{ClassKind, ConstExpr, FunctionDecl, Name, Parameter, Type, Visibility};

use crate::api::abi::{Callable, Entry};
use crate::api::coerce::{coerce_param, to_string};
use crate::api::raising::{
    Thrown, arg_count_error, arg_null_deprecated, arg_type_error, deprecated, throw,
    too_few_arguments, warning,
};
use crate::api::request::Request;
use crate::api::values::{ClassId, Key, PhpArray, PhpString, TypeTag, Value, ValueRef};

mod backtrace;
mod classes;
mod errors;
mod functions;
mod stub;

/// The engine's own natives, each declaration beside its entry: the
/// ones of `zend_builtin_functions.c` first, in the order its stub
/// declares them, which is the order `get_defined_functions()` and
/// `get_extension_funcs('Core')` list them in; then the ones php keeps
/// in `ext/standard` and this engine here, in the order they were
/// declared.
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let mut natives = zend_natives();
    natives.extend(constant_natives());
    natives.extend(class_natives());
    natives.extend(classes::natives());
    natives.extend(functions::natives());
    natives.extend(errors::natives());
    natives.extend(backtrace::natives());
    natives.sort_by_key(|(decl, _)| {
        CORE.iter()
            .position(|name| name.as_bytes() == decl.name.0.as_ref())
            .unwrap_or(CORE.len())
    });
    natives
}

/// `zend_builtin_functions.stub.php`'s functions in its order,
/// `zend_thread_id`, which a build without `ZEND_DEBUG` and `ZTS` does
/// not declare, left out.
const CORE: &[&str] = &[
    "clone",
    "exit",
    "die",
    "zend_version",
    "func_num_args",
    "func_get_arg",
    "func_get_args",
    "strlen",
    "strcmp",
    "strncmp",
    "strcasecmp",
    "strncasecmp",
    "error_reporting",
    "define",
    "defined",
    "get_class",
    "get_called_class",
    "get_parent_class",
    "is_subclass_of",
    "is_a",
    "get_class_vars",
    "get_object_vars",
    "get_mangled_object_vars",
    "get_class_methods",
    "method_exists",
    "property_exists",
    "class_exists",
    "interface_exists",
    "trait_exists",
    "enum_exists",
    "function_exists",
    "class_alias",
    "get_included_files",
    "get_required_files",
    "trigger_error",
    "user_error",
    "set_error_handler",
    "restore_error_handler",
    "get_error_handler",
    "set_exception_handler",
    "restore_exception_handler",
    "get_exception_handler",
    "get_declared_classes",
    "get_declared_traits",
    "get_declared_interfaces",
    "get_defined_functions",
    "get_defined_vars",
    "get_resource_type",
    "get_resource_id",
    "get_resources",
    "get_loaded_extensions",
    "get_defined_constants",
    "debug_backtrace",
    "debug_print_backtrace",
    "extension_loaded",
    "get_extension_funcs",
    "gc_mem_caches",
    "gc_collect_cycles",
    "gc_enabled",
    "gc_enable",
    "gc_disable",
    "gc_status",
];

/// The two that read the linked table (#259), `clone`, which reads an
/// object's handler table (#281), and `exit` with `die`, in the order
/// `zend_builtin_functions_arginfo.h` has them: `die` is
/// `ZEND_RAW_FENTRY("die", zif_exit, arginfo_die, ...)`, the same body
/// and parameters under a second name.
fn class_natives() -> Vec<(FunctionDecl, Entry)> {
    let status = Parameter {
        default: Some(ConstExpr::Long(0)),
        ..parameter(
            b"status",
            Some(Type::Union(vec![Type::String, Type::Int])),
            false,
        )
    };
    vec![
        (
            declared(
                b"clone",
                &[
                    parameter(b"object", Some(Type::Object), false),
                    Parameter {
                        default: Some(ConstExpr::Array(Vec::new())),
                        ..parameter(b"withProperties", Some(Type::Array), false)
                    },
                ],
                Some(Type::Object),
            ),
            clone as Entry,
        ),
        (
            declared(b"exit", std::slice::from_ref(&status), Some(Type::Never)),
            exit as Entry,
        ),
        (
            declared(b"die", std::slice::from_ref(&status), Some(Type::Never)),
            exit as Entry,
        ),
        (
            declared(
                b"class_exists",
                &[
                    parameter(b"class", Some(Type::String), false),
                    Parameter {
                        default: Some(ConstExpr::Bool(true)),
                        ..parameter(b"autoload", Some(Type::Bool), false)
                    },
                ],
                Some(Type::Bool),
            ),
            class_exists as Entry,
        ),
        (
            declared(
                b"get_class",
                &[parameter(b"object", Some(Type::Object), false)],
                Some(Type::String),
            ),
            get_class as Entry,
        ),
    ]
}

/// The ten of `zend_builtin_functions.c` and `ext/standard` that need
/// the engine's frames, allocator and ini table.
/// `define` and `defined`, the constant natives of
/// `zend_builtin_functions.stub.php`.
fn constant_natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            declared(
                b"define",
                &[
                    parameter(b"constant_name", Some(Type::String), false),
                    parameter(b"value", Some(Type::Mixed), false),
                    Parameter {
                        default: Some(ConstExpr::Bool(false)),
                        ..parameter(b"case_insensitive", Some(Type::Bool), false)
                    },
                ],
                Some(Type::Bool),
            ),
            define as Entry,
        ),
        (
            declared(
                b"defined",
                &[parameter(b"constant_name", Some(Type::String), false)],
                Some(Type::Bool),
            ),
            defined as Entry,
        ),
    ]
}

fn zend_natives() -> Vec<(FunctionDecl, Entry)> {
    let mut natives = vec![
        (
            declared(
                b"var_dump",
                &[parameter(b"value", None, true)],
                Some(Type::Void),
            ),
            var_dump as Entry,
        ),
        (
            declared(
                b"memory_get_usage",
                &[real_usage_parameter()],
                Some(Type::Int),
            ),
            memory_get_usage as Entry,
        ),
        (
            declared(
                b"memory_get_peak_usage",
                &[real_usage_parameter()],
                Some(Type::Int),
            ),
            memory_get_peak_usage as Entry,
        ),
        (
            declared(b"memory_reset_peak_usage", &[], Some(Type::Void)),
            memory_reset_peak_usage as Entry,
        ),
        (
            declared(
                b"extension_loaded",
                &[parameter(b"extension", Some(Type::String), false)],
                Some(Type::Bool),
            ),
            extension_loaded as Entry,
        ),
    ];
    natives.extend(gc_natives());
    natives.extend(ini_natives());
    natives.extend([
        (
            declared(
                b"strlen",
                &[parameter(b"string", Some(Type::String), false)],
                Some(Type::Int),
            ),
            strlen as Entry,
        ),
        (
            declared(
                b"register_shutdown_function",
                &[
                    parameter(b"callback", Some(Type::Callable), false),
                    parameter(b"args", Some(Type::Mixed), true),
                ],
                Some(Type::Void),
            ),
            register_shutdown_function as Entry,
        ),
    ]);
    natives
}

/// The six that read and drive the cycle collector (design §5.12),
/// each `ZEND_PARSE_PARAMETERS_NONE`.
fn gc_natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            declared(b"gc_mem_caches", &[], Some(Type::Int)),
            gc_mem_caches as Entry,
        ),
        (
            declared(b"gc_collect_cycles", &[], Some(Type::Int)),
            gc_collect_cycles as Entry,
        ),
        (
            declared(b"gc_enabled", &[], Some(Type::Bool)),
            gc_enabled as Entry,
        ),
        (
            declared(b"gc_enable", &[], Some(Type::Void)),
            gc_enable as Entry,
        ),
        (
            declared(b"gc_disable", &[], Some(Type::Void)),
            gc_disable as Entry,
        ),
        (
            declared(b"gc_status", &[], Some(Type::Array)),
            gc_status as Entry,
        ),
    ]
}

/// `ZEND_PARSE_PARAMETERS_NONE`: an argument is php's
/// `ArgumentCountError`.
fn no_arguments(req: &mut Request) -> Result<(), Thrown> {
    let count = req.argument_count();
    if count != 0 {
        return Err(wrong_argument_count(req, 0, 0, count));
    }
    Ok(())
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
fn gc_mem_caches(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let freed = req.heap_mut().gc_mem_caches();
    Ok(Value::int(as_long(freed)))
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
fn gc_collect_cycles(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    Ok(Value::int(req.gc_collect()))
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
fn gc_enabled(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    Ok(Value::bool(req.gc_enabled()))
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
fn gc_enable(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    req.gc_enable(true);
    Ok(Value::null())
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
fn gc_disable(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    req.gc_enable(false);
    Ok(Value::null())
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
fn gc_status(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    Ok(Value::array(req.gc_status()?))
}

/// The four that read and move the engine's ini table and the word
/// beside it: `ini_get`, `ini_set`, `set_time_limit` and
/// `error_reporting`.
fn ini_natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            declared(
                b"ini_get",
                &[parameter(b"option", Some(Type::String), false)],
                Some(Type::Union(vec![Type::String, Type::False])),
            ),
            ini_get as Entry,
        ),
        (
            declared(
                b"ini_set",
                &[
                    parameter(b"option", Some(Type::String), false),
                    parameter(
                        b"value",
                        Some(Type::Union(vec![
                            Type::String,
                            Type::Int,
                            Type::Float,
                            Type::Bool,
                            Type::Null,
                        ])),
                        false,
                    ),
                ],
                Some(Type::Union(vec![Type::String, Type::False])),
            ),
            ini_set as Entry,
        ),
        (
            declared(
                b"set_time_limit",
                &[parameter(b"seconds", Some(Type::Int), false)],
                Some(Type::Bool),
            ),
            set_time_limit as Entry,
        ),
        (
            declared(
                b"error_reporting",
                &[Parameter {
                    default: Some(ConstExpr::Null),
                    ..parameter(
                        b"error_level",
                        Some(Type::Nullable(Box::new(Type::Int))),
                        false,
                    )
                }],
                Some(Type::Int),
            ),
            error_reporting as Entry,
        ),
    ]
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
fn clone(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if !(1..=2).contains(&count) {
        return Err(wrong_argument_count(req, 1, 2, count));
    }
    let given = req.argument(1).unwrap_or_default().deref();
    let Some(object) = given.as_object().copied() else {
        let tail = format!(
            "must be of type object, {} given",
            crate::ops::value_name(req, &given)
        );
        return Err(arg_type_error(req, 1, &tail));
    };
    let with = if count == 2 {
        let with = req.argument(2).unwrap_or_default();
        coerce_param(req, &with, &Type::Array, 2)?
            .deref()
            .as_array()
            .copied()
    } else {
        None
    };
    req.clone_object(object, with)
        .map(|cloned| cloned.as_value())
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
fn exit(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count > 1 {
        return Err(wrong_argument_count(req, 0, 1, count));
    }
    let status = if count == 0 {
        Value::int(0)
    } else {
        string_or_long_argument(req, 1)?
    };
    Err(crate::api::raising::exit(req, &status))
}

/// `class_exists(string $class, bool $autoload = true): bool`, php's
/// `class_exists_impl` (`Z/Zend/zend_builtin_functions.c:1136`) with
/// `ZEND_ACC_LINKED` to have and `ZEND_ACC_INTERFACE | ZEND_ACC_TRAIT`
/// to lack: the class under the lowered name, a leading `\` dropped,
/// which is a class or an enum and not an interface or a trait. The
/// autoloaders are served for a miss under `$autoload`, and none is
/// registered yet.
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
fn class_exists(req: &mut Request) -> Result<Value, Thrown> {
    classes::exists(req, &[ClassKind::Class, ClassKind::Enum])
}

/// `get_class(object $object = UNKNOWN): string`, php's
/// `ZEND_FUNCTION(get_class)` (`Z/Zend/zend_builtin_functions.c:721`):
/// the declared name of the object's class, `ce->name`; without an
/// argument the executed scope's name, deprecated since 8.3, and php's
/// `Error` where there is no scope. The scope is
/// `zend_get_executed_scope`'s, which walks past this native's own
/// frame to the method that called it.
///
/// ```php
/// class M { function f() { return get_class(); } }
/// var_dump((new M)->f());
/// ```
/// ```expect
/// Deprecated: Calling get_class() without arguments is deprecated in %s on line 2
/// string(1) "M"
/// ```
///
/// ```php
/// class M {} $m = new M;
/// var_dump(get_class($m), get_class(new Exception));
/// get_class(1);
/// ```
/// ```expect exit=255
/// string(1) "M"
/// string(9) "Exception"
///
/// Fatal error: Uncaught TypeError: get_class(): Argument #1 ($object) must be of type object, int given in %s:4
/// Stack trace:
/// #0 %s(4): get_class(1)
/// #1 {main}
///   thrown in %s on line 4
/// ```
///
/// ```php
/// get_class();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: get_class() without arguments must be called from within a class in %s:2
/// Stack trace:
/// #0 %s(2): get_class()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn get_class(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count > 1 {
        return Err(wrong_argument_count(req, 0, 1, count));
    }
    let class = if count == 0 {
        match req.scope() {
            Some(scope) => {
                deprecated(req, "Calling get_class() without arguments is deprecated");
                scope
            }
            None => {
                return Err(throw(
                    req,
                    "Error",
                    "get_class() without arguments must be called from within a class",
                ));
            }
        }
    } else {
        let given = req.argument(1).unwrap_or_default();
        let object = coerce_param(req, &given, &Type::Object, 1)?;
        match object.as_object() {
            Some(object) => object.class(),
            None => return Ok(Value::null()),
        }
    };
    let name = req.class_name(class);
    Ok(Value::string(PhpString::new(req, name.as_bytes())?))
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
fn strlen(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let string = string_argument(req, 1)?;
    Ok(Value::int(as_long(string.len())))
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
/// (`copy_constant_array`); every other value takes one more owner. The
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
fn define(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if !(2..=3).contains(&count) {
        return Err(wrong_argument_count(req, 2, 3, count));
    }
    let name = string_argument(req, 1)?;
    let value = req.argument(2).unwrap_or_default().deref();
    let ignored = if count == 3 {
        let given = req.argument(3).unwrap_or_default();
        coerce_param(req, &given, &Type::Bool, 3)?
            .as_bool()
            .unwrap_or(false)
    } else {
        false
    };
    let bytes = name.as_bytes();
    if bytes.windows(2).any(|pair| pair == b"::") {
        return Err(throw(
            req,
            "ValueError",
            "define(): Argument #1 ($constant_name) cannot be a class constant",
        ));
    }
    if ignored {
        warning(
            req,
            "define(): Argument #3 ($case_insensitive) is ignored since declaration of case-insensitive constants is no longer supported",
        );
    }
    let value = if let Some(array) = value.as_array() {
        if !constant_array_is_acyclic(*array) {
            return Err(throw(
                req,
                "ValueError",
                "define(): Argument #2 ($value) cannot be a recursive array",
            ));
        }
        Value::array(copy_constant_array(req, *array)?)
    } else {
        req.addref(&value);
        value
    };
    let mut key = bytes.to_vec();
    if let Some(slash) = key.iter().rposition(|b| *b == b'\\') {
        key[..slash].make_ascii_lowercase();
    }
    let special = key == b"__COMPILER_HALT_OFFSET__"
        || key.eq_ignore_ascii_case(b"true")
        || key.eq_ignore_ascii_case(b"false")
        || key.eq_ignore_ascii_case(b"null");
    if !special && req.define_constant(&key, value)? {
        return Ok(Value::bool(true));
    }
    let shown = key
        .iter()
        .position(|b| *b == 0)
        .map_or(&key[..], |nul| &key[..nul]);
    warning(
        req,
        &format!(
            "Constant {} already defined, this will be an error in PHP 9",
            String::from_utf8_lossy(shown)
        ),
    );
    req.release(value);
    Ok(Value::bool(false))
}

/// `validate_constant_array_argument`: no array in the value, through
/// any reference, is one the walk is already inside.
fn constant_array_is_acyclic(array: PhpArray) -> bool {
    if array.is_recursive() {
        return false;
    }
    array.protect();
    let acyclic = array.snapshot().iter().all(|(_, v)| {
        v.as_array()
            .is_none_or(|nested| constant_array_is_acyclic(*nested))
    });
    array.unprotect();
    acyclic
}

/// `copy_constant_array`: a new array of the request's with the same
/// keys, each value with its reference followed, a nested array copied
/// the same way and everything else one more owner.
fn copy_constant_array(req: &mut Request, array: PhpArray) -> Result<PhpArray, Thrown> {
    let mut copy = PhpArray::new(req)?;
    for (key, value) in array.snapshot() {
        let value = if let Some(nested) = value.as_array() {
            Value::array(copy_constant_array(req, *nested)?)
        } else {
            req.addref(&value);
            value
        };
        if let Key::Str(s) = &key {
            req.addref(&Value::string(*s));
        }
        copy.set(req, key, value)?;
    }
    Ok(copy)
}

/// `defined(string $constant_name): bool`, php's `ZEND_FUNCTION(defined)`
/// (`Z/Zend/zend_builtin_functions.c:608`), which is
/// `zend_get_constant_ex` under `ZEND_FETCH_CLASS_SILENT`: a leading
/// `\` is dropped; a `::` names a class constant, which waits with the
/// classes (`zend_get_class_constant_ex`) and is `false` here; a `\`
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
/// var_dump(defined(token_get_all("<?php 1;")));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: defined(): Argument #1 ($constant_name) must be of type string, array given in %s:2
/// Stack trace:
/// #0 %s(2): defined(Array)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn defined(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let name = string_argument(req, 1)?;
    let mut bytes = name.as_bytes();
    if let Some(rest) = bytes.strip_prefix(b"\\") {
        bytes = rest;
    }
    if let Some(colon) = bytes.iter().rposition(|b| *b == b':')
        && colon > 0
        && bytes.get(colon - 1) == Some(&b':')
    {
        return Ok(Value::bool(false));
    }
    if let Some(separator) = bytes.iter().rposition(|b| *b == b'\\') {
        let (namespace, constant) = bytes.split_at(separator);
        let mut lowered = namespace.to_ascii_lowercase();
        lowered.extend_from_slice(constant);
        return Ok(Value::bool(req.constant_named(&lowered).is_some()));
    }
    Ok(Value::bool(crate::ops::defined(req, bytes)))
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
fn var_dump(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count == 0 {
        return Err(too_few_arguments(req, 1, 0));
    }
    for position in 1..=u32::try_from(count).unwrap_or(u32::MAX) {
        let value = req.argument(position).unwrap_or_default();
        let mut out = Vec::new();
        dump(req, &mut out, &value, 1);
        req.write(&out);
    }
    Ok(Value::null())
}

/// One value as `php_var_dump(zv, level)` prints it, with its newline:
/// `level - 1` blanks before it past the first level, and an array's
/// elements each through `php_array_element_dump`, the key at `level + 1`
/// and the value at `level + 2`.
///
/// The array and the object arms carry php's recursion guard: the walk
/// marks what it is inside with `Z_PROTECT_RECURSION_P` and clears the
/// mark on the way out, and a value already marked is `*RECURSION*` in
/// place of its contents, printed after the indent this level already
/// wrote and before the `&` a reference would have carried, since php
/// returns there ahead of `COMMON`. Without it a value that holds
/// itself is unbounded recursion, which is a panic reaching a request
/// (design §10).
fn dump(req: &mut Request, out: &mut Vec<u8>, value: &Value, level: usize) {
    // `COMMON`: php's `&` stands before the value a shared cell holds and
    // before nothing else, so a cell one name holds prints as the value
    // does; a cell is never in a cell, so this is read once.
    let common = match value.as_ref() {
        ValueRef::Reference(cell) if cell.heap().refcount() > 1 => "&",
        _ => "",
    };
    let value = &value.deref();
    if level > 1 {
        out.extend(std::iter::repeat_n(b' ', level - 1));
    }
    if let Some(b) = value.as_bool() {
        out.extend_from_slice(format!("{common}bool({b})\n").as_bytes());
    } else if let Some(n) = value.as_int() {
        out.extend_from_slice(format!("{common}int({n})\n").as_bytes());
    } else if let Some(f) = value.as_float() {
        // `php_var_dump` prints a float with `%.*H` at `serialize_precision`.
        let text =
            php_heap::double_to_string(f, req.serialize_precision(), php_heap::Exponent::Upper);
        out.extend_from_slice(format!("{common}float({text})\n").as_bytes());
    } else if let Some(s) = value.as_php_string() {
        out.extend_from_slice(format!("{common}string({}) \"", s.len()).as_bytes());
        out.extend_from_slice(s.as_bytes());
        out.extend_from_slice(b"\"\n");
    } else if let Some(array) = value.as_array() {
        // `GC_IS_RECURSIVE` and `GC_PROTECT_RECURSION`, which php takes
        // only where the array is refcounted; an immutable one is marked
        // by neither, here and there, since nothing can make one hold
        // itself.
        if array.is_recursive() {
            out.extend_from_slice(b"*RECURSION*\n");
            return;
        }
        array.protect();
        out.extend_from_slice(format!("{common}array({}) {{\n", array.len()).as_bytes());
        for (key, element) in array {
            out.extend(std::iter::repeat_n(b' ', level + 1));
            match key {
                Key::Int(n) => out.extend_from_slice(format!("[{n}]=>\n").as_bytes()),
                Key::Str(s) => {
                    out.extend_from_slice(b"[\"");
                    out.extend_from_slice(s.as_bytes());
                    out.extend_from_slice(b"\"]=>\n");
                }
            }
            dump(req, out, &element, level + 2);
        }
        array.unprotect();
        if level > 1 {
            out.extend(std::iter::repeat_n(b' ', level - 1));
        }
        out.extend_from_slice(b"}\n");
    } else if let Some(object) = value.as_object() {
        dump_object(req, out, common, *object, level);
    } else if let Some(resource) = value.as_resource() {
        // `IS_RESOURCE`: the list's id and the type's name through
        // `zend_rsrc_list_get_rsrc_type`, which is `Unknown` once
        // closed.
        let name = resource.type_name(req).map_or_else(
            || String::from("Unknown"),
            |name| String::from_utf8_lossy(name).into_owned(),
        );
        out.extend_from_slice(
            format!("{common}resource({}) of type ({name})\n", resource.id()).as_bytes(),
        );
    } else {
        out.extend_from_slice(format!("{common}NULL\n").as_bytes());
    }
}

/// `php_var_dump`'s `IS_OBJECT` arm: `object(Class)#handle (count) {`,
/// then each property as `php_object_property_dump` prints it, the
/// declared ones in declaration order with the visibility php mangles
/// into the name, `["p":protected]` and `["q":"Class":private]`, and a
/// typed one nothing has written as `uninitialized(type)`, which is in
/// the listing and not in the count (`zend_array_count` over a table
/// with an empty `IS_INDIRECT`); then the dynamic ones.
///
/// php marks the object before it reads its properties and clears the
/// mark after the last one, so an object reached twice down one path is
/// `*RECURSION*` the second time and an object reached twice side by
/// side is printed in full both times.
fn dump_object(
    req: &mut Request,
    out: &mut Vec<u8>,
    common: &str,
    object: crate::api::values::PhpObject,
    level: usize,
) {
    // `php_var_dump`'s `ZEND_ACC_ENUM` arm, ahead of the recursion
    // guard as php has it: a case is its class and its case name,
    // `zend_enum_fetch_case_name`, and none of its properties.
    if req
        .class_decl(object.class())
        .is_some_and(|decl| decl.kind == ClassKind::Enum)
    {
        let class = req.class_name(object.class());
        let case = object
            .prop(req, b"name")
            .and_then(|name| name.as_str_bytes().map(<[u8]>::to_vec))
            .unwrap_or_default();
        out.extend_from_slice(
            format!(
                "{common}enum({class}::{})\n",
                String::from_utf8_lossy(&case)
            )
            .as_bytes(),
        );
        return;
    }
    if object.heap().is_recursive() {
        out.extend_from_slice(b"*RECURSION*\n");
        return;
    }
    object.heap().protect();
    // `zend_get_properties_for(struc, ZEND_PROP_PURPOSE_DEBUG)`: a class
    // that shows a table of its own shows it here, which is what
    // `__debugInfo` and a `Closure` do. Every other class shows its
    // members, which are the slots walked below, where a typed property
    // nothing has written still shows as one.
    let own = match object.debug_info(req) {
        Ok(own) => own,
        Err(thrown) => {
            object.heap().unprotect();
            req.leave_pending(&thrown);
            return;
        }
    };
    if let Some(own) = own {
        dump_rows(req, out, common, object, own, level);
    } else {
        dump_members(req, out, common, object, level);
    }
    object.heap().unprotect();
}

/// `php_var_dump`'s object arm over the members themselves, which is
/// what every class that shows no table of its own shows: the declared
/// slots in `ce->properties_info_table` order, each name with the
/// visibility php spells beside it, then the dynamic ones, and a typed
/// slot nothing has written as `uninitialized(T)`, which is the row
/// php's `prop_info` keeps for an `IS_UNDEF` value.
fn dump_members(
    req: &mut Request,
    out: &mut Vec<u8>,
    common: &str,
    object: crate::api::values::PhpObject,
    level: usize,
) {
    let class = object.class();
    // `ce->properties_info_table` in slot order, which is what
    // `rebuild_object_properties_internal` walks: an inherited property
    // before the class's own, and a private one named by the class that
    // declared it (`zend_mangle_property_name`).
    let declared: Vec<(Vec<u8>, ClassId, Visibility, Option<Type>)> = req
        .placed_properties(class)
        .into_iter()
        .map(|(declaring, p)| (p.name.0.to_vec(), declaring, p.visibility, p.ty))
        .collect();
    // Each row's key as php's property table holds it, mangled by
    // `zend_mangle_property_name` for a declared one, so every name is
    // printed through the one unmangling below.
    let mut rows: Vec<(Vec<u8>, Option<Value>, Option<Type>)> = declared
        .iter()
        .enumerate()
        .filter_map(|(at, (name, declaring, visibility, ty))| {
            let value = object.slot(u32::try_from(at).unwrap_or(u32::MAX));
            if value.is_none() && ty.is_none() {
                return None;
            }
            let mangled = match visibility {
                Visibility::Public => name.clone(),
                Visibility::Protected => [&b"\0*\0"[..], name].concat(),
                Visibility::Private => {
                    let class = req.class_name(*declaring);
                    [&b"\0"[..], class.as_bytes(), b"\0", name].concat()
                }
            };
            Some((mangled, value, ty.clone()))
        })
        .collect();
    let dynamic: Vec<(PhpString, Value)> = object.props(req).unwrap_or_default();
    for (name, value) in dynamic {
        if declared
            .iter()
            .any(|(declared, _, _, _)| declared == name.as_bytes())
        {
            continue;
        }
        rows.push((name.as_bytes().to_vec(), Some(value), None));
    }
    let count = rows.iter().filter(|(_, value, _)| value.is_some()).count();
    let class_name = req.class_name(class);
    out.extend_from_slice(
        format!(
            "{common}object({class_name})#{} ({count}) {{\n",
            object.index()
        )
        .as_bytes(),
    );
    for (name, value, ty) in rows {
        dump_property_name(req, out, level, &name);
        if let Some(value) = value {
            dump(req, out, &value, level + 2);
        } else {
            out.extend(std::iter::repeat_n(b' ', level + 1));
            let ty = ty.as_ref().map(type_text).unwrap_or_default();
            out.extend_from_slice(format!("uninitialized({ty})\n").as_bytes());
        }
    }
    if level > 1 {
        out.extend(std::iter::repeat_n(b' ', level - 1));
    }
    out.extend_from_slice(b"}\n");
}

/// `php_object_property_dump`'s string key: the indent, then the name
/// between `[` and `]=>` as `zend_unmangle_property_name` reads it,
/// `"p":protected` for a class part of `*`, `"p":"Class":private` for
/// any other, and `"p"` for a name with no class part; the property and
/// the class are printed with `%s`, so each stops at a `\0` of its own.
/// A name php cannot unmangle is its bytes in quotes, after the notice
/// `zend_unmangle_property_name_ex` raises for it, which php prints
/// where the row is, ahead of the indent, so what the walk has rendered
/// so far is written before the notice is.
fn dump_property_name(req: &mut Request, out: &mut Vec<u8>, level: usize, name: &[u8]) {
    let unmangled = unmangle_property_name(name);
    if let Unmangled::Illegal(notice) = unmangled {
        req.write(out);
        out.clear();
        crate::api::raising::notice(req, notice);
    }
    out.extend(std::iter::repeat_n(b' ', level + 1));
    out.extend_from_slice(b"[\"");
    match unmangled {
        Unmangled::Mangled { class, prop } => {
            out.extend_from_slice(c_string(prop));
            if class.first() == Some(&b'*') {
                out.extend_from_slice(b"\":protected");
            } else {
                out.extend_from_slice(b"\":\"");
                out.extend_from_slice(c_string(class));
                out.extend_from_slice(b"\":private");
            }
        }
        Unmangled::Plain | Unmangled::Illegal(_) => {
            out.extend_from_slice(name);
            out.extend_from_slice(b"\"");
        }
    }
    out.extend_from_slice(b"]=>\n");
}

/// What `zend_unmangle_property_name_ex` answers for one name.
#[derive(Clone, Copy)]
enum Unmangled<'a> {
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
fn unmangle_property_name(name: &[u8]) -> Unmangled<'_> {
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

/// What `%s` prints of `bytes`: up to its first `\0`.
fn c_string(bytes: &[u8]) -> &[u8] {
    bytes
        .iter()
        .position(|&b| b == 0)
        .map_or(bytes, |at| &bytes[..at])
}

/// `php_var_dump`'s object arm over the table a class showed in place
/// of its properties: the header with the table's count, then each row
/// as `php_object_property_dump` prints one, `[0]=>` for an integer key
/// and `["name"]=>` for a string one, with the value under it. A string
/// key is unmangled as any property name is, so a table that carries
/// `\0*\0p` shows `["p":protected]`. The table has one owner and is
/// given up last.
fn dump_rows(
    req: &mut Request,
    out: &mut Vec<u8>,
    common: &str,
    object: crate::api::values::PhpObject,
    rows: PhpArray,
    level: usize,
) {
    let class_name = req.class_name(object.class());
    out.extend_from_slice(
        format!(
            "{common}object({class_name})#{} ({}) {{\n",
            object.index(),
            rows.len()
        )
        .as_bytes(),
    );
    for (key, value) in rows.snapshot() {
        match &key {
            Key::Int(at) => {
                out.extend(std::iter::repeat_n(b' ', level + 1));
                out.extend_from_slice(format!("[{at}]=>\n").as_bytes());
            }
            Key::Str(name) => dump_property_name(req, out, level, name.as_bytes()),
        }
        dump(req, out, &value, level + 2);
    }
    req.release(Value::array(rows));
    if level > 1 {
        out.extend(std::iter::repeat_n(b' ', level - 1));
    }
    out.extend_from_slice(b"}\n");
}

/// `zend_type_to_string` over a declared type: the scalar names, a
/// class name as written, `?T`, and `|` and `&` between the members of
/// a union and an intersection.
fn type_text(ty: &Type) -> String {
    match ty {
        Type::Mixed => String::from("mixed"),
        Type::Void => String::from("void"),
        Type::Never => String::from("never"),
        Type::Null => String::from("null"),
        Type::Bool => String::from("bool"),
        Type::False => String::from("false"),
        Type::True => String::from("true"),
        Type::Int => String::from("int"),
        Type::Float => String::from("float"),
        Type::String => String::from("string"),
        Type::Array => String::from("array"),
        Type::Object => String::from("object"),
        Type::Iterable => String::from("iterable"),
        Type::Callable => String::from("callable"),
        Type::CalledClass => String::from("static"),
        Type::SelfClass => String::from("self"),
        Type::ParentClass => String::from("parent"),
        Type::Named(name) => String::from_utf8_lossy(&name.0).into_owned(),
        Type::Nullable(inner) => format!("?{}", type_text(inner)),
        Type::Union(members) => members.iter().map(type_text).collect::<Vec<_>>().join("|"),
        Type::Intersection(members) => members.iter().map(type_text).collect::<Vec<_>>().join("&"),
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
/// on the witness of `php_heap::string::Table` records.
///
/// ```php
/// var_dump(memory_get_usage(true), memory_get_peak_usage(true));
/// memory_reset_peak_usage();
/// var_dump(memory_get_peak_usage(true));
/// ```
/// ```expect
/// int(2097152)
/// int(2097152)
/// int(2097152)
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
fn memory_get_usage(req: &mut Request) -> Result<Value, Thrown> {
    let real = real_usage(req)?;
    Ok(Value::int(as_long(req.memory_usage(real))))
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
fn memory_get_peak_usage(req: &mut Request) -> Result<Value, Thrown> {
    let real = real_usage(req)?;
    Ok(Value::int(as_long(req.memory_peak(real))))
}

/// `memory_reset_peak_usage(): void`, php's
/// `zend_memory_reset_peak_usage`: both peaks become what is in use now.
/// `ZEND_PARSE_PARAMETERS_NONE` takes no argument at all, so one is
/// php's `ArgumentCountError` and not an ignored extra.
fn memory_reset_peak_usage(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 0 {
        return Err(wrong_argument_count(req, 0, 0, count));
    }
    req.reset_memory_peak();
    Ok(Value::null())
}

/// php's `ZEND_PARSE_PARAMETERS_START(0, 1)` over one optional
/// `Z_PARAM_BOOL`: no argument is `false`, a second is php's
/// `ArgumentCountError`, and what is there is coerced as php coerces a
/// bool parameter of a native, `null` included, which is the
/// 8.1 deprecation and then `false`. The two shared helpers this reaches
/// for read `$real_usage` off the declaration, so their text is php's.
fn real_usage(req: &mut Request) -> Result<bool, Thrown> {
    let count = req.argument_count();
    if count > 1 {
        return Err(wrong_argument_count(req, 0, 1, count));
    }
    let Some(given) = req.argument(1) else {
        return Ok(false);
    };
    if matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
        arg_null_deprecated(req, 1, "bool");
        return Ok(false);
    }
    let coerced = coerce_param(req, &given, &Type::Bool, 1)?;
    Ok(coerced.as_bool().unwrap_or(false))
}

/// `zend_wrong_parameters_count_error` for a call outside a native's
/// arity: `exactly` where the maximum is also the minimum, else `at
/// least` under it and `at most` over it, the bound the call missed, and
/// that bound's own plural.
fn wrong_argument_count(req: &mut Request, least: usize, most: usize, given: usize) -> Thrown {
    let how = if least == most {
        "exactly"
    } else if given < least {
        "at least"
    } else {
        "at most"
    };
    let bound = if given < least { least } else { most };
    let word = if bound == 1 { "argument" } else { "arguments" };
    arg_count_error(req, &format!("{how} {bound} {word}, {given} given"))
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
fn extension_loaded(req: &mut Request) -> Result<Value, Thrown> {
    let Some(given) = req.argument(1) else {
        return Err(too_few_arguments(req, 1, 0));
    };
    let wanted = crate::ops::to_string(req, &given)?;
    let lowered = wanted.as_bytes().to_ascii_lowercase();
    req.release(Value::string(wanted));
    Ok(Value::bool(req.loaded_extensions().iter().any(|name| {
        name.as_bytes().to_ascii_lowercase() == lowered
    })))
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
fn ini_get(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let option = string_argument(req, 1)?;
    let value = req.ini_get(option.as_bytes()).map(<[u8]>::to_vec);
    Ok(match value {
        Some(value) => Value::string(PhpString::new(req, &value)?),
        None => Value::bool(false),
    })
}

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
fn ini_set(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 2 {
        return Err(wrong_argument_count(req, 2, 2, count));
    }
    let option = string_argument(req, 1)?;
    let value = req.argument(2).unwrap_or_default();
    let value = &value.deref();
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
    let set = req.ini_set(option.as_bytes(), value.as_bytes());
    req.release(Value::string(value));
    Ok(match set? {
        Some(previous) => Value::string(PhpString::new(req, &previous)?),
        None => Value::bool(false),
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
fn set_time_limit(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let seconds = long_argument(req, 1)?;
    Ok(Value::bool(req.set_time_limit(seconds)))
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
/// int(32767)
/// int(32767)
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
/// string(5) "32767"
/// int(2)
/// string(2) "2x"
/// int(0)
/// ```
fn error_reporting(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count > 1 {
        return Err(wrong_argument_count(req, 0, 1, count));
    }
    let old = req.error_reporting();
    if count == 1 {
        let given = req.argument(1).unwrap_or_default();
        if !matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
            let level = coerce_param(req, &given, &Type::Int, 1)?
                .as_int()
                .unwrap_or(0);
            if level != old {
                req.alter_error_reporting(level);
            }
        }
    }
    Ok(Value::int(old))
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
fn register_shutdown_function(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count == 0 {
        return Err(too_few_arguments(req, 1, 0));
    }
    let callback = callable_argument(req, 1)?;
    req.addref(&callback.value());
    let mut arguments = Vec::new();
    for position in 2..=u32::try_from(count).unwrap_or(u32::MAX) {
        let value = req.argument(position).unwrap_or_default();
        req.addref(&value);
        arguments.push(value);
    }
    req.register_shutdown_function(callback, arguments);
    Ok(Value::null())
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
    while let Some((callback, arguments)) = req.shutdown_function(at) {
        at += 1;
        match crate::api::call::call(req, &callback, &arguments) {
            Ok(value) => req.release(value),
            Err(thrown) => {
                req.leave_pending(&thrown);
                if let Err(fatal) = req.uncaught() {
                    req.end_in(&fatal);
                }
                return;
            }
        }
    }
}

/// One `Z_PARAM_FUNC` of a native, php's `"F"`: the argument at
/// `position` resolved into the callee it names, `zend_parse_arg_func`
/// over `zend_is_callable_ex`, so a callback is held to being one where
/// it is taken and not where it is served. The sentence a miss takes is
/// php's `*error` for that miss, the one `zend_is_callable_ex` wrote.
fn callable_argument(req: &mut Request, position: u32) -> Result<Callable, Thrown> {
    let given = req.argument(position).unwrap_or_default().deref();
    match req.callable_checked(&given) {
        Ok(callable) => Ok(callable),
        Err(why) => Err(arg_type_error(
            req,
            position,
            &format!("be a valid callback, {why}"),
        )),
    }
}

/// One `Z_PARAM_LONG` of a native, the argument at `position` read off
/// the call frame: `null` is the 8.1 deprecation and then zero, and what
/// is there is coerced as php coerces an int parameter.
fn long_argument(req: &mut Request, position: u32) -> Result<i64, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    if matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
        arg_null_deprecated(req, position, "int");
        return Ok(0);
    }
    let coerced = coerce_param(req, &given, &Type::Int, position)?;
    Ok(coerced.as_int().unwrap_or(0))
}

/// One `Z_PARAM_STR` of a native, the argument at `position` read off
/// the call frame: `null` is the 8.1 deprecation and then the empty
/// string, and what is there is coerced as php coerces a string
/// parameter. The string is the frame's, a view the native holds while
/// the call stands.
fn string_argument(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    if matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
        arg_null_deprecated(req, position, "string");
        return PhpString::new(req, b"");
    }
    let coerced = coerce_param(req, &given, &Type::String, position)?;
    match coerced.as_php_string() {
        Some(s) => Ok(*s),
        None => PhpString::new(req, b""),
    }
}

/// One `Z_PARAM_STR_OR_LONG` of a native, the argument at `position`
/// read off the call frame (`zend_parse_arg_str_or_long`,
/// `Z/Zend/zend_API.h`): a string or an int as it is, and under the weak
/// rule `zend_parse_arg_str_or_long_slow`'s two tries in order, the
/// int's (`null` the 8.1 deprecation and then `0`, a bool its int, a
/// float the int it truncates to where it fits) and then the string's
/// (a float past the long's range as its string, an object through
/// `__toString`); what neither takes, and everything but a string or
/// an int under `strict_types`, is php's `TypeError` naming the union.
fn string_or_long_argument(req: &mut Request, position: u32) -> Result<Value, Thrown> {
    let given = req.argument(position).unwrap_or_default().deref();
    if matches!(given.as_ref(), ValueRef::Str(_) | ValueRef::Int(_)) {
        return Ok(given);
    }
    if !req.strict_types() {
        match given.as_ref() {
            ValueRef::Null | ValueRef::Undef => {
                arg_null_deprecated(req, position, "string|int");
                return Ok(Value::int(0));
            }
            ValueRef::Bool(b) => return Ok(Value::int(i64::from(b))),
            ValueRef::Float(_) => {
                if let Some(n) = crate::ops::arg_long_weak(req, &given) {
                    return Ok(Value::int(n));
                }
                return Ok(Value::string(to_string(req, &given)?));
            }
            ValueRef::Object(object) => {
                if let Some(text) = req.cast_object(*object, TypeTag::String)? {
                    return Ok(text);
                }
            }
            _ => {}
        }
    }
    let tail = format!(
        "be of type string|int, {} given",
        crate::ops::value_name(req, &given)
    );
    Err(arg_type_error(req, position, &tail))
}

/// One declared parameter.
fn parameter(name: &[u8], ty: Option<Type>, variadic: bool) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty,
        by_ref: false,
        prefer_ref: false,
        variadic,
        default: None,
        attributes: Vec::new(),
    }
}

/// `bool $real_usage = false`, the one parameter the two memory readers
/// share.
fn real_usage_parameter() -> Parameter {
    Parameter {
        default: Some(ConstExpr::Bool(false)),
        ..parameter(b"real_usage", Some(Type::Bool), false)
    }
}

/// One native's declaration, as the macros derive an extension's from
/// its Rust signature.
fn declared(name: &[u8], parameters: &[Parameter], returns: Option<Type>) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.into()),
        parameters: parameters.to_vec(),
        return_type: returns,
        returns_by_ref: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
        frameless: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::Engine;
    use crate::test::Capture;

    /// What the engine wrote and what ended it, for one source.
    fn served(source: &str) -> (String, Option<String>) {
        let served = Engine::builder()
            .build()
            .serve_capture(source, Capture::default());
        (
            String::from_utf8_lossy(&served.stdout).into_owned(),
            served.fatal,
        )
    }

    /// php's `ZEND_PARSE_PARAMETERS_START(0, 1)`: `$real_usage` is
    /// optional and `false` without it, and a scalar is coerced as a
    /// bool parameter of a native is.
    ///
    /// The oracle, under the pin's recipe:
    ///
    /// ```text
    /// $ php -n -r 'var_dump(memory_get_usage(true) === memory_get_usage(1));'
    /// bool(true)
    /// ```
    ///
    /// **The flagged number is the oracle's; the unflagged one is this
    /// engine's own accounting.** 2097152 is php's one 2 MiB chunk,
    /// `ZEND_MM_CHUNK_SIZE` (`Z/Zend/zend_alloc_sizes.h:22`), which
    /// `zend_mm_init` maps into `real_size` before a request runs a line
    /// (`Z/Zend/zend_alloc.c:2047-2081`), and the oracle answers it too.
    ///
    /// 49240 is the object store's table and what this source's
    /// interning costs, and every byte of it is php's own arithmetic.
    /// `init_executor` lays `EG(objects_store)` down before the request
    /// serves a line, 1024 buckets of a pointer each, 8192 bytes of the
    /// request heap (`Z/Zend/zend_execute_API.c:171`, design §5.4), and
    /// so does this engine's request. php interns a request's constant operands
    /// into `CG(interned_strings)` out of the request heap
    /// (`zend_insert_literal` through `zend_new_interned_string_request`,
    /// `Z/Zend/zend_compile.c:563-582`, `Z/Zend/zend_string.c:227-265`),
    /// so its `memory_get_usage()` counts them and this engine's counts
    /// them the same way (design §5.2, §5.13): `var_dump` takes 40 bytes
    /// and `memory_get_usage` 48, php's
    /// `ZEND_MM_ALIGNED_SIZE(_ZSTR_STRUCT_SIZE(len))` over a 24-byte
    /// header, the bytes and the NUL. The 40960 beside them is the
    /// table's own bucket array, which php `emalloc`s on the first string
    /// a request interns and not at request start: 1024 buckets of 32
    /// bytes and 2048 hash slots of 4, `HT_SIZE_EX(1024,
    /// HT_SIZE_TO_MASK(1024))` (`Z/Zend/zend_types.h:486-493`) over the
    /// `nTableSize` `zend_interned_strings_activate` sets
    /// (`Z/Zend/zend_string.c:78-84`, `:346-349`).
    ///
    /// The oracle's number for the same source, under the recipe and the
    /// harness's own scratch tree, is 463336, and the two are still not
    /// comparable: what php's request heap holds beside the interned
    /// strings and their table is here on the system allocator, the op
    /// array, the compiler and the tree, which design §5.13 states as an
    /// approximation. The witness on `php_heap::string::Table` holds the
    /// two numbers to each other under the divergence row that says so
    /// (#178). What replaces this pin with a number both answer is #174's
    /// witness, where a variable in `php_compile` and a `str_repeat()`
    /// native let a snippet measure what one huge block moves
    /// `memory_get_usage()` by.
    #[test]
    fn the_real_usage_flag_is_optional_and_coerced_as_php_coerces_a_bool() {
        let (unflagged, _) = served("<?php var_dump(memory_get_usage());");
        let (flagged, _) = served("<?php var_dump(memory_get_usage(true));");
        let (coerced, _) = served("<?php var_dump(memory_get_usage(1));");
        assert_eq!(
            unflagged, "int(49240)\n",
            "the object store's 8192, the interned table's 40960 and the two strings in it, 40 and 48"
        );
        assert_eq!(flagged, "int(2097152)\n");
        assert_eq!(coerced, flagged, "php's Z_PARAM_BOOL coerces 1 to true");
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
    /// has out. The witness beside `memory_get_usage` pins the flagged
    /// number against the oracle; this holds the reset when no oracle is
    /// on the machine, and holds the unflagged number beside it.
    ///
    /// **The second number is the oracle's; the first is this engine's
    /// own accounting**, for the reason the pin above states at length.
    /// 2097152 is php's one 2 MiB chunk. 49288 is the object store's
    /// 8192 and what this source's interning costs the request: the
    /// table's own 40960 bucket array on the first string it interns,
    /// and then
    /// `memory_reset_peak_usage` 48 bytes, `var_dump` 40 and
    /// `memory_get_peak_usage` 48. php counts its own interning the same
    /// way and then counts a request heap this engine does not have yet,
    /// so its number for this source, under the recipe as `witness.php`,
    /// is 463752 and not this one; #174's witness is where a number both
    /// answer comes back.
    #[test]
    fn a_reset_leaves_the_peak_at_what_the_request_has_out() {
        let (body, fatal) = served(
            "<?php memory_reset_peak_usage(); var_dump(memory_get_peak_usage(), memory_get_peak_usage(true));",
        );
        assert_eq!(fatal, None);
        assert_eq!(
            body, "int(49288)\nint(2097152)\n",
            "the object store's 8192, the interned table's 40960 and the three strings in it, 48, 40 and 48"
        );
    }
}
