//! The natives the engine declares itself (design §13): the Zend
//! builtins that need frames or the engine's own tables, which php-src
//! keeps in `ext/standard` and this engine keeps here, so an engine with
//! no extensions serves a program that uses Zend alone (design §4.10).
//!
//! Seven of them are here: the two the first witness of a loop needs, the
//! three that read what `php_heap::mm` accounts, so the allocator's
//! numbers are php's numbers under a witness and not under a terminal
//! session a person typed once, `ini_set`, which moves the engine's own
//! ini table so a witness can reach what an entry decides, and
//! `set_time_limit`, which moves the one entry the execution timer is
//! armed from. Each is declared and paired with its entry exactly as an
//! extension's are, so there is one shape for a native and not two.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_unit::decl::{ConstExpr, FunctionDecl, Name, Parameter, Type};

use crate::api::abi::Entry;
use crate::api::coerce::{coerce_param, to_string};
use crate::api::raising::{
    Thrown, arg_count_error, arg_null_deprecated, arg_type_error, too_few_arguments,
};
use crate::api::request::Request;
use crate::api::values::{PhpString, Value, ValueRef};

/// The engine's own natives, in the order php declares them, each
/// declaration beside its entry.
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
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
    ]
}

/// `var_dump(mixed $value, mixed ...$values): void`, php's
/// `php_var_dump`: each value's type and contents, one line each.
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
fn var_dump(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count == 0 {
        return Err(too_few_arguments(req, 1, 0));
    }
    for position in 1..=u32::try_from(count).unwrap_or(u32::MAX) {
        let value = req.argument(position).unwrap_or_default();
        let line = dumped(req, &value);
        req.write(line.as_bytes());
    }
    Ok(Value::null())
}

/// One value as `php_var_dump` prints it, with its newline.
fn dumped(req: &mut Request, value: &Value) -> PhpString {
    let value = value.deref();
    if let Some(b) = value.as_bool() {
        return PhpString::from_bytes(format!("bool({b})\n").as_bytes());
    }
    if let Some(n) = value.as_int() {
        return PhpString::from_bytes(format!("int({n})\n").as_bytes());
    }
    if let Some(f) = value.as_float() {
        // `php_var_dump` prints a float with `%.*H` at `serialize_precision`.
        let text = crate::ops::double_to_string(f, req.serialize_precision());
        return PhpString::from_bytes(
            format!("float({})\n", String::from_utf8_lossy(text.as_bytes())).as_bytes(),
        );
    }
    if let Some(s) = value.as_php_string() {
        let mut line = PhpString::from_bytes(format!("string({}) \"", s.len()).as_bytes());
        line.append(s.as_bytes());
        line.append(b"\"\n");
        return line;
    }
    PhpString::from_bytes(b"NULL\n")
}

/// `memory_get_usage(bool $real_usage = false): int`, php's
/// `zend_memory_usage`: the bin accounting the request has out, or under
/// `$real_usage` the bytes its chunks hold.
///
/// A fresh request holds php's one 2 MiB chunk and nothing more, so the
/// second number is that chunk and the oracle says so. The first is each
/// engine's own bookkeeping and is not pinned here; what
/// a huge block moves it by is `php_heap`'s
/// `a_huge_block_rounds_to_the_page_the_machine_reports`, which this
/// witness cannot yet reach (#174).
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
fn memory_get_usage(req: &mut Request) -> Result<Value, Thrown> {
    let real = real_usage(req)?;
    Ok(Value::int(as_long(req.memory_usage(real))))
}

/// `memory_get_peak_usage(bool $real_usage = false): int`, php's
/// `zend_memory_peak_usage`: the high-water mark of the same two
/// numbers since the request began or the peak was last reset.
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
    let wanted = crate::ops::to_string(req, &given);
    let wanted = wanted.as_bytes().to_ascii_lowercase();
    Ok(Value::bool(req.loaded_extensions().iter().any(|name| {
        name.as_bytes().to_ascii_lowercase() == wanted
    })))
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
    let value = value.deref();
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
    Ok(match req.ini_set(option.as_bytes(), value.as_bytes())? {
        Some(previous) => Value::string(previous),
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
/// parameter.
fn string_argument(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    if matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
        arg_null_deprecated(req, position, "string");
        return Ok(PhpString::default());
    }
    let coerced = coerce_param(req, &given, &Type::String, position)?;
    Ok(coerced.as_php_string().cloned().unwrap_or_default())
}

/// One declared parameter.
fn parameter(name: &[u8], ty: Option<Type>, variadic: bool) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty,
        by_ref: false,
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
    /// 41048 is what this source's interning costs, and every byte of it
    /// is php's own arithmetic. php interns a request's constant operands
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
    /// approximation. The witness on `php_heap::string::Buckets` holds the
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
            unflagged, "int(41048)\n",
            "the interned table's 40960 and the two strings in it, 40 and 48"
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
    /// 2097152 is php's one 2 MiB chunk. 41096 is what this source's
    /// interning costs the request: the table's own 40960 bucket array on
    /// the first string it interns, and then
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
            body, "int(41096)\nint(2097152)\n",
            "the interned table's 40960 and the three strings in it, 48, 40 and 48"
        );
    }
}
