//! The natives of `zend_builtin_functions.c` that read the calling
//! frame, the function, constant and resource tables and the bundle
//! of extensions, and the four binary-safe string comparisons: the calling
//! frame is `EX(prev_execute_data)`, which [`Request::native_caller`]
//! reads off the one chain a backtrace walks.

use php_unit::decl::{FunctionDecl, Type};

use super::stub::stub;
use super::{CORE, long_argument, string_argument, wrong_argument_count};
use crate::api::abi::Entry;
use crate::api::coerce::coerce_param;
use crate::api::raising::{Thrown, arg_type_error, arg_value_error, deprecated, throw};
use crate::api::request::{NativeCaller, Request};
use crate::api::values::{Key, PhpArray, PhpString, Resource, TypeTag, Value, ValueRef};

/// The nineteen, in the stub's order.
pub(super) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (stub("zend_version(): string"), zend_version as Entry),
        (stub("func_num_args(): int"), func_num_args as Entry),
        (
            stub("func_get_arg(int $position): mixed"),
            func_get_arg as Entry,
        ),
        (stub("func_get_args(): array"), func_get_args as Entry),
        (
            stub("strcmp(string $string1, string $string2): int"),
            strcmp as Entry,
        ),
        (
            stub("strncmp(string $string1, string $string2, int $length): int"),
            strncmp as Entry,
        ),
        (
            stub("strcasecmp(string $string1, string $string2): int"),
            strcasecmp as Entry,
        ),
        (
            stub("strncasecmp(string $string1, string $string2, int $length): int"),
            strncasecmp as Entry,
        ),
        (
            stub("function_exists(string $function): bool"),
            function_exists as Entry,
        ),
        (
            stub("get_included_files(): array"),
            get_included_files as Entry,
        ),
        (
            stub("get_required_files(): array"),
            get_included_files as Entry,
        ),
        (
            stub("get_defined_functions(bool $exclude_disabled = true): array"),
            get_defined_functions as Entry,
        ),
        (stub("get_defined_vars(): array"), get_defined_vars as Entry),
        (
            stub("get_resource_type($resource): string"),
            get_resource_type as Entry,
        ),
        (
            stub("get_resource_id($resource): int"),
            get_resource_id as Entry,
        ),
        (
            stub("get_resources(?string $type = null): array"),
            get_resources as Entry,
        ),
        (
            stub("get_loaded_extensions(bool $zend_extensions = false): array"),
            get_loaded_extensions as Entry,
        ),
        (
            stub("get_defined_constants(bool $categorize = false): array"),
            get_defined_constants as Entry,
        ),
        (
            stub("get_extension_funcs(string $extension): array|false"),
            get_extension_funcs as Entry,
        ),
    ]
}

/// `ZEND_VERSION` at the pin (`Z/Zend/zend.h:23`).
const ZEND_VERSION: &str = "4.5.9";

/// `ZEND_PARSE_PARAMETERS_NONE()`.
fn no_arguments(req: &mut Request) -> Result<(), Thrown> {
    let count = req.argument_count();
    if count == 0 {
        return Ok(());
    }
    Err(wrong_argument_count(req, 0, 0, count))
}

/// One `Z_PARAM_BOOL` of a native, coerced as php coerces a bool
/// parameter.
fn bool_argument(req: &mut Request, position: u32) -> Result<bool, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    Ok(coerce_param(req, &given, &Type::Bool, position)?
        .as_bool()
        .unwrap_or(false))
}

/// `zend_version(): string`, php's `ZEND_FUNCTION(zend_version)`
/// (`Z/Zend/zend_builtin_functions.c:146`).
///
/// ```php
/// var_dump(zend_version());
/// ```
/// ```expect
/// string(5) "4.5.9"
/// ```
fn zend_version(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    Ok(Value::string(PhpString::new(req, ZEND_VERSION.as_bytes())?))
}

/// The calling function's arguments, or php's `Error` for a caller that
/// is none: a code frame is `what` said of it, and another native is
/// `zend_forbid_dynamic_call`'s. The name is the native's own.
fn caller_arguments(req: &mut Request, name: &str, what: &str) -> Result<Vec<Value>, Thrown> {
    match req.native_caller() {
        NativeCaller::Function(_, arguments) => Ok(arguments),
        NativeCaller::Code(_) => Err(throw(req, "Error", &format!("{name}() {what}"))),
        NativeCaller::Native => Err(throw(
            req,
            "Error",
            &format!("Cannot call {name}() dynamically"),
        )),
    }
}

/// `func_num_args(): int`, php's `ZEND_FUNCTION(func_num_args)`
/// (`Z/Zend/zend_builtin_functions.c:236`): `ZEND_CALL_NUM_ARGS` of the
/// calling function, every argument it was passed. php's compiler
/// serves a call of it inside a function with `ZEND_FUNC_NUM_ARGS`;
/// what reaches the native is a call outside one, the `Error`.
///
/// ```php
/// function f($a, $b = 2) { return func_num_args(); }
/// var_dump(f(1), f(1, 2, 3));
/// func_num_args();
/// ```
/// ```expect exit=255
/// int(1)
/// int(3)
///
/// Fatal error: Uncaught Error: func_num_args() must be called from a function context in %s:4
/// Stack trace:
/// #0 %s(4): func_num_args()
/// #1 {main}
///   thrown in %s on line 4
/// ```
fn func_num_args(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let arguments = caller_arguments(
        req,
        "func_num_args",
        "must be called from a function context",
    )?;
    Ok(Value::int(
        i64::try_from(arguments.len()).unwrap_or(i64::MAX),
    ))
}

/// `func_get_arg(int $position): mixed`, php's
/// `ZEND_FUNCTION(func_get_arg)` (`Z/Zend/zend_builtin_functions.c:254`):
/// the argument at the zero-based position as the calling function's
/// slot holds it now, its reference followed, `null` for a slot nothing
/// holds; a position past what was passed, or below zero, is php's
/// `ValueError`.
///
/// ```php
/// function f($a, $b = 2) { $a = 9; return [func_get_arg(0), func_get_arg(2)]; }
/// var_dump(f(1, 2, 3));
/// function g() { return func_get_arg(1); }
/// g(1);
/// ```
/// ```expect exit=255
/// array(2) {
///   [0]=>
///   int(9)
///   [1]=>
///   int(3)
/// }
///
/// Fatal error: Uncaught ValueError: func_get_arg(): Argument #1 ($position) must be less than the number of the arguments passed to the currently executed function in %s:4
/// Stack trace:
/// #0 %s(4): func_get_arg(1)
/// #1 %s(5): g(1)
/// #2 {main}
///   thrown in %s on line 4
/// ```
fn func_get_arg(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let position = long_argument(req, 1)?;
    if position < 0 {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    }
    let arguments = caller_arguments(
        req,
        "func_get_arg",
        "cannot be called from the global scope",
    )?;
    let Some(argument) = usize::try_from(position)
        .ok()
        .and_then(|at| arguments.get(at))
    else {
        return Err(arg_value_error(
            req,
            1,
            "be less than the number of the arguments passed to the currently executed function",
        ));
    };
    let value = argument.deref();
    if value.tag() == TypeTag::Undef {
        return Ok(Value::null());
    }
    req.addref(&value);
    Ok(value)
}

/// `func_get_args(): array`, php's `ZEND_FUNCTION(func_get_args)`
/// (`Z/Zend/zend_builtin_functions.c:295`): every argument the calling
/// function was passed, as its slots hold them now, a reference
/// followed and a slot nothing holds `null`. php's compiler serves a
/// call of it inside a function with `ZEND_FUNC_GET_ARGS`; what reaches
/// the native outside one is the `Error`.
///
/// ```php
/// function f($a, ...$rest) { $a = 9; return func_get_args(); }
/// var_dump(f(1, 2, 3), f(1));
/// func_get_args();
/// ```
/// ```expect exit=255
/// array(3) {
///   [0]=>
///   int(9)
///   [1]=>
///   int(2)
///   [2]=>
///   int(3)
/// }
/// array(1) {
///   [0]=>
///   int(9)
/// }
///
/// Fatal error: Uncaught Error: func_get_args() cannot be called from the global scope in %s:4
/// Stack trace:
/// #0 %s(4): func_get_args()
/// #1 {main}
///   thrown in %s on line 4
/// ```
fn func_get_args(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let arguments = caller_arguments(
        req,
        "func_get_args",
        "cannot be called from the global scope",
    )?;
    let mut array = PhpArray::new(req)?;
    for argument in arguments {
        let value = argument.deref();
        let value = if value.tag() == TypeTag::Undef {
            Value::null()
        } else {
            req.addref(&value);
            value
        };
        array.append(req, value)?;
    }
    Ok(Value::array(array))
}

/// `memcmp`'s answer on this host, which php hands back as it is:
/// the difference of the first two bytes that differ.
fn memcmp(a: &[u8], b: &[u8]) -> i64 {
    a.iter()
        .zip(b)
        .find(|(x, y)| x != y)
        .map_or(0, |(x, y)| i64::from(*x) - i64::from(*y))
}

/// `ZEND_THREEWAY_COMPARE`.
fn threeway(a: usize, b: usize) -> i64 {
    match a.cmp(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `zend_binary_strncmp` (`Z/Zend/zend_operators.c:3259`), and
/// `zend_binary_strcmp` with no length: `memcmp` over the shorter, and
/// the lengths compared where the bytes agree.
fn binary_strncmp(a: &[u8], b: &[u8], length: usize) -> i64 {
    let (a, b) = (&a[..a.len().min(length)], &b[..b.len().min(length)]);
    match memcmp(a, b) {
        0 => threeway(a.len(), b.len()),
        differ => differ,
    }
}

/// `zend_binary_strncasecmp` (`Z/Zend/zend_operators.c:3297`): the bytes
/// lowered by `zend_tolower_ascii`, the first pair that differs
/// answered by its difference, and the lengths where none does.
fn binary_strncasecmp(a: &[u8], b: &[u8], length: usize) -> i64 {
    let (a, b) = (&a[..a.len().min(length)], &b[..b.len().min(length)]);
    a.iter()
        .zip(b)
        .map(|(x, y)| (x.to_ascii_lowercase(), y.to_ascii_lowercase()))
        .find(|(x, y)| x != y)
        .map_or_else(
            || threeway(a.len(), b.len()),
            |(x, y)| i64::from(x) - i64::from(y),
        )
}

/// The two strings and, where `with_length`, the length a comparison
/// reads, which is php's `ValueError` below zero.
fn compared(req: &mut Request, with_length: bool) -> Result<(Vec<u8>, Vec<u8>, usize), Thrown> {
    let want = if with_length { 3 } else { 2 };
    let count = req.argument_count();
    if count != want {
        return Err(wrong_argument_count(req, want, want, count));
    }
    let a = string_argument(req, 1)?.as_bytes().to_vec();
    let b = string_argument(req, 2)?.as_bytes().to_vec();
    if !with_length {
        return Ok((a, b, usize::MAX));
    }
    let length = long_argument(req, 3)?;
    let Ok(length) = usize::try_from(length) else {
        return Err(arg_value_error(req, 3, "be greater than or equal to 0"));
    };
    Ok((a, b, length))
}

/// `strcmp(string $string1, string $string2): int`, php's
/// `ZEND_FUNCTION(strcmp)` (`Z/Zend/zend_builtin_functions.c:387`) over
/// `zend_binary_strcmp`: `memcmp`'s own answer where the bytes differ,
/// which on this host is the difference of the first two that do, and
/// `-1`, `0` or `1` from the lengths where one string begins the other.
///
/// ```php
/// var_dump(strcmp("a", "c"), strcmp("c", "a"), strcmp("ab", "a"), strcmp("a", "ab"), strcmp("", ""), strcmp("a\0b", "a\0c"));
/// ```
/// ```expect
/// int(-2)
/// int(2)
/// int(1)
/// int(-1)
/// int(0)
/// int(-1)
/// ```
fn strcmp(req: &mut Request) -> Result<Value, Thrown> {
    let (a, b, length) = compared(req, false)?;
    Ok(Value::int(binary_strncmp(&a, &b, length)))
}

/// `strncmp(string $string1, string $string2, int $length): int`, php's
/// `ZEND_FUNCTION(strncmp)` over `zend_binary_strncmp`: the first
/// `$length` bytes of each.
///
/// ```php
/// var_dump(strncmp("abcd", "abcz", 3), strncmp("abcd", "abcz", 4), strncmp("a", "b", 0), strncmp("ab", "abc", 5));
/// strncmp("a", "b", -1);
/// ```
/// ```expect exit=255
/// int(0)
/// int(-22)
/// int(0)
/// int(-1)
///
/// Fatal error: Uncaught ValueError: strncmp(): Argument #3 ($length) must be greater than or equal to 0 in %s:3
/// Stack trace:
/// #0 %s(3): strncmp('a', 'b', -1)
/// #1 {main}
///   thrown in %s on line 3
/// ```
fn strncmp(req: &mut Request) -> Result<Value, Thrown> {
    let (a, b, length) = compared(req, true)?;
    Ok(Value::int(binary_strncmp(&a, &b, length)))
}

/// `strcasecmp(string $string1, string $string2): int`, php's
/// `ZEND_FUNCTION(strcasecmp)` over `zend_binary_strcasecmp`: ASCII
/// letters compared lowered.
///
/// ```php
/// var_dump(strcasecmp("HELLO", "hello"), strcasecmp("HELLO", "hellz"), strcasecmp("b", "A"), strcasecmp("\xC9", "\xE9"));
/// ```
/// ```expect
/// int(0)
/// int(-11)
/// int(1)
/// int(-32)
/// ```
fn strcasecmp(req: &mut Request) -> Result<Value, Thrown> {
    let (a, b, length) = compared(req, false)?;
    Ok(Value::int(binary_strncasecmp(&a, &b, length)))
}

/// `strncasecmp(string $string1, string $string2, int $length): int`,
/// php's `ZEND_FUNCTION(strncasecmp)` over `zend_binary_strncasecmp`.
///
/// ```php
/// var_dump(strncasecmp("ABx", "aby", 2), strncasecmp("ABx", "aby", 3), strncasecmp("a", "A", 0));
/// strncasecmp("a", "b", -5);
/// ```
/// ```expect exit=255
/// int(0)
/// int(-1)
/// int(0)
///
/// Fatal error: Uncaught ValueError: strncasecmp(): Argument #3 ($length) must be greater than or equal to 0 in %s:3
/// Stack trace:
/// #0 %s(3): strncasecmp('a', 'b', -5)
/// #1 {main}
///   thrown in %s on line 3
/// ```
fn strncasecmp(req: &mut Request) -> Result<Value, Thrown> {
    let (a, b, length) = compared(req, true)?;
    Ok(Value::int(binary_strncasecmp(&a, &b, length)))
}

/// `function_exists(string $function): bool`, php's
/// `ZEND_FUNCTION(function_exists)` (`Z/Zend/zend_builtin_functions.c:1188`):
/// the lowered name, a leading `\` dropped, in the function table,
/// which holds the natives and every user function the request
/// declared by now.
///
/// ```php
/// function f() {}
/// var_dump(function_exists("strlen"), function_exists("\\STRLEN"), function_exists("F"), function_exists("g"), function_exists("nope"));
/// function g() {}
/// if (true) { function h() {} }
/// var_dump(function_exists("h"));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
fn function_exists(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let name = string_argument(req, 1)?;
    let bytes = name.as_bytes();
    let bytes = bytes.strip_prefix(b"\\").unwrap_or(bytes);
    Ok(Value::bool(req.function_named(bytes).is_some()))
}

/// `get_included_files(): array`, php's
/// `ZEND_FUNCTION(get_included_files)` (`Z/Zend/zend_builtin_functions.c:1256`),
/// and `get_required_files`, its alias: every path `EG(included_files)`
/// holds, the primary file's first and each included one as it was
/// resolved.
///
/// ```php
/// $f = __DIR__ . "/get_included_files.inc"; file_put_contents($f, "<?php return 1;"); include $f;
/// $files = get_included_files(); var_dump(str_ends_with($files[count($files) - 1], "/get_included_files.inc"), get_required_files() === $files);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
fn get_included_files(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let mut array = PhpArray::new(req)?;
    for path in req.included_files() {
        let path = Value::string(PhpString::new(req, &path)?);
        array.append(req, path)?;
    }
    Ok(Value::array(array))
}

/// `get_defined_functions(bool $exclude_disabled = true): array`, php's
/// `ZEND_FUNCTION(get_defined_functions)` (`Z/Zend/zend_builtin_functions.c:1496`):
/// `internal` and `user`, each the lowered keys of the function table
/// in its order; the parameter has done nothing since 8.0 and passing
/// it is the deprecation.
///
/// ```php
/// function MyFunc() {} $c = function () {};
/// $all = get_defined_functions();
/// var_dump($all['internal'][0], $all['user'], count(get_defined_functions(false)));
/// ```
/// ```expect
/// Deprecated: get_defined_functions(): The $exclude_disabled parameter has no effect since PHP 8.0 in %s on line 4
/// string(5) "clone"
/// array(1) {
///   [0]=>
///   string(6) "myfunc"
/// }
/// int(2)
/// ```
fn get_defined_functions(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count > 1 {
        return Err(wrong_argument_count(req, 0, 1, count));
    }
    if count == 1 {
        bool_argument(req, 1)?;
        deprecated(
            req,
            "get_defined_functions(): The $exclude_disabled parameter has no effect since PHP 8.0",
        );
    }
    Ok(Value::array(req.defined_functions()?))
}

/// `get_defined_vars(): array`, php's `ZEND_FUNCTION(get_defined_vars)`
/// (`Z/Zend/zend_builtin_functions.c:1528`): the calling frame's symbol
/// table, rebuilt for a function frame that has none, copied: every
/// variable that holds a value, a reference kept as one.
///
/// ```php
/// function g() { return get_defined_vars(); }
/// function h($x) { $y = 2; $z = &$x; $unset = null; unset($unset); return get_defined_vars(); }
/// var_dump(g(), h(1));
/// ```
/// ```expect
/// array(0) {
/// }
/// array(3) {
///   ["x"]=>
///   &int(1)
///   ["y"]=>
///   int(2)
///   ["z"]=>
///   &int(1)
/// }
/// ```
fn get_defined_vars(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let frame = match req.native_caller() {
        NativeCaller::Function(frame, _) | NativeCaller::Code(frame) => frame,
        NativeCaller::Native => {
            return Err(throw(
                req,
                "Error",
                "Cannot call get_defined_vars() dynamically",
            ));
        }
    };
    Ok(Value::array(req.defined_vars(frame)?))
}

/// One `Z_PARAM_RESOURCE` of a native, the resource or php's `TypeError`.
fn resource_argument(req: &mut Request, position: u32) -> Result<Resource, Thrown> {
    let given = req.argument(position).unwrap_or_default().deref();
    if let Some(resource) = given.as_resource() {
        return Ok(*resource);
    }
    let tail = format!(
        "be of type resource, {} given",
        crate::ops::value_name(req, &given)
    );
    Err(arg_type_error(req, position, &tail))
}

/// `get_resource_type($resource): string`, php's
/// `ZEND_FUNCTION(get_resource_type)` (`Z/Zend/zend_builtin_functions.c:1546`):
/// the name its type registered under, and `Unknown` once it is closed.
///
/// ```php
/// $f = fopen('php://memory', 'r'); var_dump(get_resource_type($f)); fclose($f); var_dump(get_resource_type($f));
/// get_resource_type(1);
/// ```
/// ```expect exit=255
/// string(6) "stream"
/// string(7) "Unknown"
///
/// Fatal error: Uncaught TypeError: get_resource_type(): Argument #1 ($resource) must be of type resource, int given in %s:3
/// Stack trace:
/// #0 %s(3): get_resource_type(1)
/// #1 {main}
///   thrown in %s on line 3
/// ```
fn get_resource_type(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let resource = resource_argument(req, 1)?;
    let name = resource
        .type_name(req)
        .map_or_else(|| b"Unknown".to_vec(), <[u8]>::to_vec);
    Ok(Value::string(PhpString::new(req, &name)?))
}

/// `get_resource_id($resource): int`, php's
/// `ZEND_FUNCTION(get_resource_id)` (`Z/Zend/zend_builtin_functions.c:1565`):
/// `Z_RES_HANDLE`, the resource's index in the list.
///
/// ```php
/// $f = fopen('php://memory', 'r'); var_dump(get_resource_id($f) === (int) $f);
/// ```
/// ```expect
/// bool(true)
/// ```
fn get_resource_id(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let resource = resource_argument(req, 1)?;
    Ok(Value::int(resource.id()))
}

/// `get_resources(?string $type = null): array`, php's
/// `ZEND_FUNCTION(get_resources)` (`Z/Zend/zend_builtin_functions.c:1578`):
/// `EG(regular_list)` keyed by id, every resource, the closed ones for
/// `Unknown`, or those of the type named, a name no type registered
/// being php's `ValueError`.
///
/// ```php
/// $f = fopen('php://memory', 'r'); $g = fopen('php://memory', 'r'); fclose($g);
/// $streams = get_resources('stream'); $closed = get_resources('Unknown'); $all = get_resources();
/// var_dump($streams[(int) $f] === $f, isset($streams[(int) $g]), $closed[(int) $g] === $g, isset($all[(int) $g]));
/// get_resources('nope');
/// ```
/// ```expect exit=255
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
///
/// Fatal error: Uncaught ValueError: get_resources(): Argument #1 ($type) must be a valid resource type in %s:5
/// Stack trace:
/// #0 %s(5): get_resources('nope')
/// #1 {main}
///   thrown in %s on line 5
/// ```
fn get_resources(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count > 1 {
        return Err(wrong_argument_count(req, 0, 1, count));
    }
    let given = req.argument(1).unwrap_or_default().deref();
    let wanted = if count == 1 && !matches!(given.as_ref(), ValueRef::Null) {
        Some(string_argument(req, 1)?.as_bytes().to_vec())
    } else {
        None
    };
    let kind = match wanted.as_deref() {
        None => None,
        Some(b"Unknown") => Some(None),
        Some(name) => match req.resource_type_named(name) {
            Some(kind) => Some(Some(kind)),
            None => return Err(arg_value_error(req, 1, "be a valid resource type")),
        },
    };
    let mut array = PhpArray::new(req)?;
    for resource in req.resource_list() {
        if kind.is_some_and(|kind| kind != resource.kind()) {
            continue;
        }
        let value = resource.as_value();
        req.addref(&value);
        array.set(req, Key::Int(resource.id()), value)?;
    }
    Ok(Value::array(array))
}

/// `get_loaded_extensions(bool $zend_extensions = false): array`, php's
/// `ZEND_FUNCTION(get_loaded_extensions)` (`Z/Zend/zend_builtin_functions.c:1633`):
/// the names in `module_registry`, `Core` first; the engine loads no Zend
/// extension, so the list of those is empty.
///
/// ```php
/// $loaded = get_loaded_extensions(); $seen = [];
/// foreach ($loaded as $name) { $seen[$name] = true; }
/// var_dump($loaded[0], isset($seen['standard']), isset($seen['json']), isset($seen['nope']));
/// ```
/// ```expect
/// string(4) "Core"
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
fn get_loaded_extensions(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count > 1 {
        return Err(wrong_argument_count(req, 0, 1, count));
    }
    let zend_extensions = count == 1 && bool_argument(req, 1)?;
    let names = if zend_extensions {
        Vec::new()
    } else {
        req.loaded_extensions()
    };
    let mut array = PhpArray::new(req)?;
    for name in names {
        let name = Value::string(PhpString::new(req, name.as_bytes())?);
        array.append(req, name)?;
    }
    Ok(Value::array(array))
}

/// `get_defined_constants(bool $categorize = false): array`, php's
/// `ZEND_FUNCTION(get_defined_constants)` (`Z/Zend/zend_builtin_functions.c:1656`):
/// `EG(zend_constants)` as name and value in the table's order; with
/// `$categorize`, grouped under the extension each was registered by,
/// `user` for the program's own, each group where its first constant
/// stands.
///
/// ```php
/// define('MINE', 1); const OTHER = [2];
/// $all = get_defined_constants(); $by = get_defined_constants(true);
/// foreach ($by as $first => $_) { break; }
/// var_dump($all['E_ALL'], $all['MINE'], $first, $by['user'], $by['Core']['E_ERROR']);
/// ```
/// ```expect
/// int(30719)
/// int(1)
/// string(4) "Core"
/// array(2) {
///   ["MINE"]=>
///   int(1)
///   ["OTHER"]=>
///   array(1) {
///     [0]=>
///     int(2)
///   }
/// }
/// int(1)
/// ```
fn get_defined_constants(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count > 1 {
        return Err(wrong_argument_count(req, 0, 1, count));
    }
    let categorize = count == 1 && bool_argument(req, 1)?;
    let rows: Vec<(Option<String>, Vec<u8>, Value)> = req
        .constant_table()
        .into_iter()
        .map(|(module, name, value)| (module.map(str::to_owned), name, value))
        .collect();
    let mut array = PhpArray::new(req)?;
    if !categorize {
        for (_, name, value) in rows {
            req.addref(&value);
            let key = Key::Str(PhpString::new(req, &name)?);
            array.set(req, key, value)?;
        }
        return Ok(Value::array(array));
    }
    let mut groups: Vec<(String, Vec<Named>)> = Vec::new();
    for (module, name, value) in rows {
        let module = module.unwrap_or_else(|| String::from("user"));
        match groups.iter_mut().find(|(group, _)| *group == module) {
            Some((_, members)) => members.push((name, value)),
            None => groups.push((module, vec![(name, value)])),
        }
    }
    for (module, members) in groups {
        let mut group = PhpArray::new(req)?;
        for (name, value) in members {
            req.addref(&value);
            let key = Key::Str(PhpString::new(req, &name)?);
            group.set(req, key, value)?;
        }
        let key = Key::Str(PhpString::new(req, module.as_bytes())?);
        array.set(req, key, Value::array(group))?;
    }
    Ok(Value::array(array))
}

/// One constant of a group, its name and its value.
type Named = (Vec<u8>, Value);

/// `get_extension_funcs(string $extension): array|false`, php's
/// `ZEND_FUNCTION(get_extension_funcs)` (`Z/Zend/zend_builtin_functions.c:2269`):
/// the names of the natives the extension named declared, as declared,
/// `zend` naming `Core`; `false` for an extension the bundle has not, and
/// for one that declared none. `Core`'s are the functions of
/// `zend_builtin_functions.c`, the ones php keeps in `ext/standard`
/// that this engine declares being none of them.
///
/// ```php
/// var_dump(get_extension_funcs('zend')[3], get_extension_funcs('CORE') === get_extension_funcs('Zend'), get_extension_funcs('json')[0], get_extension_funcs('nope'));
/// ```
/// ```expect
/// string(12) "zend_version"
/// bool(true)
/// string(11) "json_encode"
/// bool(false)
/// ```
fn get_extension_funcs(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let name = string_argument(req, 1)?.as_bytes().to_vec();
    let names: Vec<Vec<u8>> =
        if name.eq_ignore_ascii_case(b"zend") || name.eq_ignore_ascii_case(b"core") {
            CORE.iter()
                .map(|name| name.as_bytes().to_vec())
                .filter(|name| req.function_named(name).is_some())
                .collect()
        } else {
            let Some(extension) = req.extension_named(&name) else {
                return Ok(Value::bool(false));
            };
            req.extension_functions(extension)
                .into_iter()
                .filter_map(|function| req.function_decl(function))
                .map(|decl| decl.name.0.to_vec())
                .collect()
        };
    if names.is_empty() {
        return Ok(Value::bool(false));
    }
    let mut array = PhpArray::new(req)?;
    for name in names {
        let name = Value::string(PhpString::new(req, &name)?);
        array.append(req, name)?;
    }
    Ok(Value::array(array))
}
