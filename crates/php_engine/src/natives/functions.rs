//! The natives of `zend_builtin_functions.c` that read the calling
//! frame, the function, constant and resource tables and the bundle
//! of extensions, and the four binary-safe string comparisons: the calling
//! frame is `EX(prev_execute_data)`, which [`Request::native_caller`]
//! reads off the one chain a backtrace walks.

use php_macros::php_function;
use php_unit::decl::Bare;

use crate::api::abi::{OrFalse, Passed};
use crate::api::raising::{Thrown, arg_value_error, deprecated, throw_error};
use crate::api::request::{NativeCaller, Request};
use crate::api::values::{Owner, PhpArray, PhpString, Resource, TypeTag, Value};

/// `ZEND_VERSION` at the pin (`Z/Zend/zend.h:23`).
pub(crate) const ZEND_VERSION: &str = "4.5.9";

/// `zend_version(): string`, php's `ZEND_FUNCTION(zend_version)`
/// (`Z/Zend/zend_builtin_functions.c:146`).
///
/// ```php
/// var_dump(zend_version());
/// ```
/// ```expect
/// string(5) "4.5.9"
/// ```
#[php_function]
fn zend_version(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, ZEND_VERSION.as_bytes())
}

/// The calling function's arguments, or php's `Error` for a caller that
/// is none: a code frame is `what` said of it, and another native is
/// `zend_forbid_dynamic_call`'s ([`Frames::forbid_dynamic_call`]). The
/// name is the native's own.
fn caller_arguments(req: &mut Request, name: &str, what: &str) -> Result<Vec<Value>, Thrown> {
    match req.native_caller() {
        NativeCaller::Function(_, arguments) => Ok(arguments),
        NativeCaller::Code(_) => Err(throw_error(req, "Error", &format!("{name}() {what}"))),
        NativeCaller::Native => Err(req.dynamic_call()),
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
#[php_function]
fn func_num_args(req: &mut Request) -> Result<i64, Thrown> {
    let arguments = caller_arguments(
        req,
        "func_num_args",
        "must be called from a function context",
    )?;
    Ok(i64::try_from(arguments.len()).unwrap_or(i64::MAX))
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
#[php_function]
fn func_get_arg(req: &mut Request, position: i64) -> Result<Value, Thrown> {
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
#[php_function]
fn func_get_args(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let arguments = caller_arguments(
        req,
        "func_get_args",
        "cannot be called from the global scope",
    )?;
    copied_arguments(req, arguments, 0)
}

/// What `func_get_args()` and `ZEND_FUNC_GET_ARGS` copy: the arguments
/// past the first `skip`, as their slots hold them now, a reference
/// followed and a slot nothing holds `null`; `zend_empty_array` where
/// none is left, which costs nothing.
pub(crate) fn copied_arguments(
    req: &mut Request,
    arguments: Vec<Value>,
    skip: usize,
) -> Result<Owner<PhpArray>, Thrown> {
    if arguments.len() <= skip {
        return Ok(Owner::take(req, PhpArray::empty()));
    }
    let mut array = PhpArray::new(req)?;
    for argument in arguments.into_iter().skip(skip) {
        let value = argument.deref();
        let value = if value.tag() == TypeTag::Undef {
            Value::null()
        } else {
            value
        };
        array.append(req, Owner::take(req, value))?;
    }
    Ok(array)
}

/// `ZEND_THREEWAY_COMPARE`.
fn threeway(a: usize, b: usize) -> i64 {
    match a.cmp(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `zend_binary_strncmp` (`Z/Zend/zend_operators.c:3259`) with `fold`
/// the identity, `memcmp` over the shorter, whose answer on this host
/// php hands back as it is; and `zend_binary_strncasecmp` (`:3297`)
/// with `fold` `zend_tolower_ascii`. Either way the first pair of bytes
/// that differ once folded is answered by its difference, and the
/// lengths are compared where none does; with no length, they are
/// `zend_binary_strcmp` and `zend_binary_strcasecmp`.
fn binary_strncmp(a: &[u8], b: &[u8], length: usize, fold: fn(&u8) -> u8) -> i64 {
    let (a, b) = (&a[..a.len().min(length)], &b[..b.len().min(length)]);
    a.iter()
        .zip(b)
        .map(|(x, y)| (fold(x), fold(y)))
        .find(|(x, y)| x != y)
        .map_or_else(
            || threeway(a.len(), b.len()),
            |(x, y)| i64::from(x) - i64::from(y),
        )
}

/// The two strings and, where `with_length`, the length a comparison
/// reads, which is php's `ValueError` below zero.
fn compared(req: &mut Request, length: i64) -> Result<usize, Thrown> {
    usize::try_from(length).map_err(|_| arg_value_error(req, 3, "be greater than or equal to 0"))
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
#[php_function]
fn strcmp(string1: PhpString, string2: PhpString) -> i64 {
    binary_strncmp(
        string1.as_bytes(),
        string2.as_bytes(),
        usize::MAX,
        u8::clone,
    )
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
#[php_function]
fn strncmp(
    req: &mut Request,
    string1: PhpString,
    string2: PhpString,
    length: i64,
) -> Result<i64, Thrown> {
    let length = compared(req, length)?;
    Ok(binary_strncmp(
        string1.as_bytes(),
        string2.as_bytes(),
        length,
        u8::clone,
    ))
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
#[php_function]
fn strcasecmp(string1: PhpString, string2: PhpString) -> i64 {
    let lower = u8::to_ascii_lowercase;
    binary_strncmp(string1.as_bytes(), string2.as_bytes(), usize::MAX, lower)
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
#[php_function]
fn strncasecmp(
    req: &mut Request,
    string1: PhpString,
    string2: PhpString,
    length: i64,
) -> Result<i64, Thrown> {
    let length = compared(req, length)?;
    Ok(binary_strncmp(
        string1.as_bytes(),
        string2.as_bytes(),
        length,
        u8::to_ascii_lowercase,
    ))
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
#[php_function]
fn function_exists(req: &mut Request, function: PhpString) -> bool {
    req.function_named(Bare::of(function.as_bytes())).is_some()
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
#[php_function(aliases(get_required_files))]
fn get_included_files(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    for path in req.included_files() {
        let path = PhpString::new(req, &path)?.into_value();
        array.append(req, path)?;
    }
    Ok(array)
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
#[php_function]
fn get_defined_functions(
    req: &mut Request,
    #[php(default = true)] exclude_disabled: Passed<bool>,
) -> Result<Owner<PhpArray>, Thrown> {
    if let Passed::Given(_) = exclude_disabled {
        deprecated(
            req,
            "get_defined_functions(): The $exclude_disabled parameter has no effect since PHP 8.0",
        );
    }
    req.symbols_mut().defined_functions()
}

/// `get_defined_vars(): array`, php's `ZEND_FUNCTION(get_defined_vars)`
/// (`Z/Zend/zend_builtin_functions.c:1517`): the calling frame's symbol
/// table, rebuilt for a function frame that has none, copied as
/// `zend_array_dup` copies it ([`Request::defined_vars`]): every
/// variable that holds a value, a reference another variable shares
/// kept as one.
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
///
/// A reference only the variable holds, what `sort()` leaves a variable
/// it took and `extract()` one it bound to an array since gone, is
/// copied as its value, so writing to the copy changes no variable:
///
/// ```php
/// function f() {
///     $a = [3, 1, 2]; sort($a);
///     $src = ['b' => [1]]; extract($src, EXTR_REFS); unset($src);
///     $vars = get_defined_vars(); $vars['a'][] = 9; $vars['b'][] = 9;
///     var_dump(count($a), count($b));
/// }
/// f();
/// ```
/// ```expect
/// int(3)
/// int(1)
/// ```
#[php_function]
fn get_defined_vars(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let frame = match req.native_caller() {
        NativeCaller::Function(frame, _) | NativeCaller::Code(frame) => frame,
        NativeCaller::Native => {
            return Err(throw_error(
                req,
                "Error",
                "Cannot call get_defined_vars() dynamically",
            ));
        }
    };
    let made = req.defined_vars(frame);
    crate::api::values::adopted(made)
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
#[php_function]
fn get_resource_type(req: &mut Request, resource: Resource) -> Result<Owner<PhpString>, Thrown> {
    let name = resource
        .type_name(req)
        .map_or_else(|| b"Unknown".to_vec(), <[u8]>::to_vec);
    PhpString::new(req, &name)
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
#[php_function]
fn get_resource_id(resource: Resource) -> i64 {
    resource.id()
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
#[php_function]
fn get_resources(
    req: &mut Request,
    #[php(default = null)] r#type: Option<PhpString>,
) -> Result<Owner<PhpArray>, Thrown> {
    let wanted = r#type.map(|name| name.as_bytes().to_vec());
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
        array.set(req, Owner::index(resource.id()), Owner::take(req, value))?;
    }
    Ok(array)
}

/// `get_loaded_extensions(bool $zend_extensions = false): array`, php's
/// `ZEND_FUNCTION(get_loaded_extensions)` (`Z/Zend/zend_builtin_functions.c:1633`):
/// the names in `module_registry`, `Core` first, or with
/// `$zend_extensions` the names in `zend_extensions`, opcache's
/// `Zend OPcache` where the bundle carries it.
///
/// ```php
/// $loaded = get_loaded_extensions(); $seen = [];
/// foreach ($loaded as $name) { $seen[$name] = true; }
/// var_dump($loaded[0], isset($seen['standard']), isset($seen['json']), isset($seen['nope']));
/// var_dump(get_loaded_extensions(true));
/// ```
/// ```expect
/// string(4) "Core"
/// bool(true)
/// bool(true)
/// bool(false)
/// array(1) {
///   [0]=>
///   string(12) "Zend OPcache"
/// }
/// ```
#[php_function]
fn get_loaded_extensions(
    req: &mut Request,
    #[php(default = false)] zend_extensions: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let names = if zend_extensions {
        req.extensions()
            .zend_extensions()
            .into_iter()
            .map(|row| row.name)
            .collect()
    } else {
        req.extensions().loaded_extensions()
    };
    let mut array = PhpArray::new(req)?;
    for name in names {
        let name = PhpString::new(req, name.as_bytes())?.into_value();
        array.append(req, name)?;
    }
    Ok(array)
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
#[php_function]
fn get_defined_constants(
    req: &mut Request,
    #[php(default = false)] categorize: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    // The table read once: every name into one buffer, each row its span
    // of it and its module's group, found by name in a map, as php finds a
    // group by its module number; the keys are made after, the request
    // being free again.
    let mut array = PhpArray::new(req)?;
    let names: Vec<u8>;
    let mut spans: Vec<(Option<usize>, usize, usize, Value)> = Vec::new();
    let mut modules: Vec<String> = Vec::new();
    {
        let mut module_at: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        let mut bytes = Vec::new();
        for (module, name, value) in req.symbols().constant_table() {
            let group = module.map(|module| {
                *module_at.entry(module).or_insert_with(|| {
                    modules.push(module.to_owned());
                    modules.len() - 1
                })
            });
            spans.push((group, bytes.len(), name.len(), value));
            bytes.extend_from_slice(name);
        }
        names = bytes;
    }
    let name_of = |start: usize, len: usize| &names[start..start + len];
    if !categorize {
        for &(_, start, len, value) in &spans {
            let key = PhpString::new(req, name_of(start, len))?.into_key();
            array.set(req, key, Owner::take(req, value))?;
        }
        return Ok(array);
    }
    // `modules`' groups in the order each first stands, then `user`'s,
    // the one php adds last for `PHP_USER_CONSTANT`.
    let user = modules.len();
    let mut groups: Vec<Vec<(usize, usize, Value)>> = vec![Vec::new(); user + 1];
    let mut order: Vec<usize> = Vec::new();
    for &(group, start, len, value) in &spans {
        let group = group.unwrap_or(user);
        if groups[group].is_empty() {
            order.push(group);
        }
        groups[group].push((start, len, value));
    }
    for group_at in order {
        let mut group = PhpArray::new(req)?;
        for &(start, len, value) in &groups[group_at] {
            let key = PhpString::new(req, name_of(start, len))?.into_key();
            group.set(req, key, Owner::take(req, value))?;
        }
        let module = modules.get(group_at).map_or("user", String::as_str);
        let key = PhpString::new(req, module.as_bytes())?.into_key();
        array.set(req, key, group.into_value())?;
    }
    Ok(array)
}

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
#[php_function]
fn get_extension_funcs(
    req: &mut Request,
    extension: PhpString,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let name = extension.as_bytes().to_vec();
    let names: Vec<Vec<u8>> =
        if name.eq_ignore_ascii_case(b"zend") || name.eq_ignore_ascii_case(b"core") {
            super::core()
                .into_iter()
                .map(|(decl, _)| decl.name.0.to_vec())
                .filter(|name| req.function_named(Bare::resolved(name)).is_some())
                .collect()
        } else {
            let Some(extension) = req.extensions().named(&name) else {
                return Ok(OrFalse::False);
            };
            req.extensions()
                .functions(extension)
                .into_iter()
                .filter_map(|function| req.symbols().function_decl(function))
                .map(|decl| decl.name.0.to_vec())
                .collect()
        };
    if names.is_empty() {
        return Ok(OrFalse::False);
    }
    let mut array = PhpArray::new(req)?;
    for name in names {
        let name = PhpString::new(req, &name)?.into_value();
        array.append(req, name)?;
    }
    Ok(OrFalse::Value(array))
}
