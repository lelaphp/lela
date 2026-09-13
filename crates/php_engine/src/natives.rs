//! The natives the engine declares itself (design §13): the Zend
//! builtins that need frames or the engine's own tables, which php-src
//! keeps in `ext/standard` and this engine keeps here, so an engine with
//! no extensions serves a program that uses Zend alone (design §4.10).
//!
//! Five of them are here: the two the first witness of a loop needs, and
//! the three that read what `php_heap::mm` accounts, so the allocator's
//! numbers are php's numbers under a witness and not under a terminal
//! session a person typed once. Each is declared and paired with its entry exactly as an
//! extension's are, so there is one shape for a native and not two.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_unit::decl::{ConstExpr, FunctionDecl, Name, Parameter, Type};

use crate::api::abi::Entry;
use crate::api::coerce::coerce_param;
use crate::api::raising::{Thrown, arg_count_error, arg_null_deprecated, too_few_arguments};
use crate::api::request::Request;
use crate::api::values::{PhpString, Value};

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
fn var_dump(req: &mut Request, arguments: &[Value]) -> Result<Value, Thrown> {
    if arguments.is_empty() {
        return Err(too_few_arguments(req, 1, 0));
    }
    for value in arguments {
        let line = dumped(req, value);
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
fn memory_get_usage(req: &mut Request, arguments: &[Value]) -> Result<Value, Thrown> {
    let real = real_usage(req, arguments)?;
    Ok(Value::int(as_long(req.memory_usage(real))))
}

/// `memory_get_peak_usage(bool $real_usage = false): int`, php's
/// `zend_memory_peak_usage`: the high-water mark of the same two
/// numbers since the request began or the peak was last reset.
fn memory_get_peak_usage(req: &mut Request, arguments: &[Value]) -> Result<Value, Thrown> {
    let real = real_usage(req, arguments)?;
    Ok(Value::int(as_long(req.memory_peak(real))))
}

/// `memory_reset_peak_usage(): void`, php's
/// `zend_memory_reset_peak_usage`: both peaks become what is in use now.
/// `ZEND_PARSE_PARAMETERS_NONE` takes no argument at all, so one is
/// php's `ArgumentCountError` and not an ignored extra.
fn memory_reset_peak_usage(req: &mut Request, arguments: &[Value]) -> Result<Value, Thrown> {
    if !arguments.is_empty() {
        return Err(wrong_argument_count(req, 0, arguments.len()));
    }
    req.reset_memory_peak();
    Ok(Value::null())
}

/// php's `ZEND_PARSE_PARAMETERS_START(0, 1)` over one optional
/// `Z_PARAM_BOOL`: no argument is `false`, a second is php's
/// `ArgumentCountError`, and what is there is coerced as php coerces a
/// bool parameter of a native, `null` included, which is the
/// 8.1 deprecation and then `false`. The two shared helpers this reaches
/// for write no parameter name, so their text is not yet php's (#175);
/// what is php's here is the arity, the optional and the coercion.
fn real_usage(req: &mut Request, arguments: &[Value]) -> Result<bool, Thrown> {
    if arguments.len() > 1 {
        return Err(wrong_argument_count(req, 1, arguments.len()));
    }
    let Some(given) = arguments.first() else {
        return Ok(false);
    };
    if matches!(
        given.deref().as_ref(),
        crate::api::values::ValueRef::Null | crate::api::values::ValueRef::Undef
    ) {
        arg_null_deprecated(req, 1, "bool");
        return Ok(false);
    }
    let coerced = coerce_param(req, given, &Type::Bool, 1)?;
    Ok(coerced.as_bool().unwrap_or(false))
}

/// `zend_wrong_parameters_count_error` for a call past a native's
/// maximum: `exactly` where the maximum is also the minimum, `at most`
/// where it is not, and the count's own plural.
fn wrong_argument_count(req: &mut Request, most: usize, given: usize) -> Thrown {
    let how = if most == 0 { "exactly" } else { "at most" };
    let word = if most == 1 { "argument" } else { "arguments" };
    arg_count_error(req, &format!("{how} {most} {word}, {given} given"))
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
fn extension_loaded(req: &mut Request, arguments: &[Value]) -> Result<Value, Thrown> {
    let Some(wanted) = arguments.first().map(|v| crate::ops::to_string(req, v)) else {
        return Err(too_few_arguments(req, 1, 0));
    };
    let wanted = wanted.as_bytes().to_ascii_lowercase();
    Ok(Value::bool(req.loaded_extensions().iter().any(|name| {
        name.as_bytes().to_ascii_lowercase() == wanted
    })))
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
    /// 88 is what the two constant strings this source compiles cost.
    /// php interns a request's constant operands into
    /// `CG(interned_strings)` out of the request heap
    /// (`zend_insert_literal` through `zend_new_interned_string_request`,
    /// `Z/Zend/zend_compile.c:563-582`, `Z/Zend/zend_string.c:227-265`),
    /// so its `memory_get_usage()` counts them and this engine's counts
    /// them the same way (design §5.2, §5.13): `var_dump` takes 40 bytes
    /// and `memory_get_usage` 48, php's
    /// `ZEND_MM_ALIGNED_SIZE(_ZSTR_STRUCT_SIZE(len))` over a 24-byte
    /// header, the bytes and the NUL.
    ///
    /// The oracle's number for the same source, under the recipe as
    /// `witness.php`, is 463400, and the two are not comparable: what
    /// php's request heap holds beside the
    /// interned strings is here on the system allocator, the table's own
    /// buckets (#178) with the op array, the compiler and the tree,
    /// which design §5.13 states as an approximation. What replaces this
    /// pin with a number both answer is #174's witness, where a variable
    /// in `php_compile` and a `str_repeat()` native let a snippet measure
    /// what one huge block moves `memory_get_usage()` by.
    #[test]
    fn the_real_usage_flag_is_optional_and_coerced_as_php_coerces_a_bool() {
        let (unflagged, _) = served("<?php var_dump(memory_get_usage());");
        let (flagged, _) = served("<?php var_dump(memory_get_usage(true));");
        let (coerced, _) = served("<?php var_dump(memory_get_usage(1));");
        assert_eq!(
            unflagged, "int(88)\n",
            "the two constant strings this source interns, 40 and 48"
        );
        assert_eq!(flagged, "int(2097152)\n");
        assert_eq!(coerced, flagged, "php's Z_PARAM_BOOL coerces 1 to true");
    }

    /// `zend_wrong_parameters_count_error` for a call past the maximum,
    /// which a witness cannot pin while the engine's fatal carries no
    /// stack trace and no line (#174 names what the witnesses here wait
    /// on). php's own text, from the pinned oracle:
    ///
    /// ```text
    /// ArgumentCountError: memory_get_usage() expects at most 1 argument, 2 given
    /// ArgumentCountError: memory_get_peak_usage() expects at most 1 argument, 2 given
    /// ArgumentCountError: memory_reset_peak_usage() expects exactly 0 arguments, 1 given
    /// ```
    #[test]
    fn a_call_past_the_maximum_is_php_s_argument_count_error() {
        for (source, text) in [
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
    /// 2097152 is php's one 2 MiB chunk. 136 is what this source's three
    /// constant strings cost the request that interned them:
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
            body, "int(136)\nint(2097152)\n",
            "the three constant strings this source interns, 48, 40 and 48"
        );
    }
}
