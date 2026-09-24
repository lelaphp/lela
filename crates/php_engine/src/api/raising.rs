//! The raising group (design §4.4): how a native reports what php
//! reports. The engine knows the active native's name and parameters
//! before the body, so the argument raisers take the position and the
//! tail of the message and the engine renders `fn(): Argument #N ($name)
//! must ...` itself. [`Thrown`] is the error every native returns. In
//! this skeleton nothing is rendered or dispatched to a user handler: a
//! diagnostic is kept on the request as its last error, and a `Thrown`
//! carries the throwable it would have made pending.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_unit::decl::{Attribute, ConstExpr};

use crate::api::request::Request;
use crate::api::values::{PhpObject, PhpString, Value};

/// php's error levels, `E_*`: the unit's, since a compile-time
/// diagnostic is recorded on the unit at one of them and raised here
/// (design §6.1).
pub use php_unit::Level;

/// What a native returns when it does not return a value: a throwable is
/// pending on the request, or the request is ending in a fatal or an
/// `exit`. A `Thrown` in hand *is* the pending state; there is no second
/// channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Thrown {
    pending: Option<PhpObject>,
}

impl Thrown {
    pub(crate) const fn of(pending: Option<PhpObject>) -> Thrown {
        Thrown { pending }
    }

    /// The throwable that is pending, if one is: `None` for a fatal or
    /// an `exit`.
    #[must_use]
    pub fn exception(&self) -> Option<&PhpObject> {
        self.pending.as_ref()
    }
}

/// Raise a diagnostic at `level` with php's message text.
pub fn raise(req: &mut Request, level: Level, message: &str) {
    req.note_error(level, message);
}

/// `zend_error_at`: the same diagnostic at a file and line of its own
/// rather than the site the request has reached, which is what php's
/// scanner and compiler raise with while a file is being compiled, the
/// compiled name and `CG(zend_lineno)`. What `token_get_all()` under
/// `TOKEN_PARSE` raises for an octal escape overflow is the witness, in
/// `ext/tokenizer`: the file is the empty one `tokenize_parse` scans
/// under, so php prints `in  on line 1`.
pub fn raise_at(req: &mut Request, level: Level, message: &str, file: &[u8], line: u32) {
    req.note_error_in(level, message, file, line);
}

/// `E_WARNING`.
pub fn warning(req: &mut Request, message: &str) {
    raise(req, Level::Warning, message);
}

/// `E_NOTICE`.
pub fn notice(req: &mut Request, message: &str) {
    raise(req, Level::Notice, message);
}

/// `E_DEPRECATED`.
pub fn deprecated(req: &mut Request, message: &str) {
    raise(req, Level::Deprecated, message);
}

/// `zend_deprecated_constant` (`Z/Zend/zend_execute.c:1994`): the
/// `E_DEPRECATED` a read of a constant the `#[\Deprecated]` attribute
/// marks raises, named by the key the table holds it under and worded
/// with [`deprecation_suffix`] after php's sentence. A constant with
/// no such attribute raises nothing, which is every constant but
/// `E_STRICT` today. php's level is `E_USER_DEPRECATED` for a
/// constant a program defined and `E_DEPRECATED` for an extension's;
/// a constant a program defined carries no attribute here (#465), so
/// only the extension's arm is reachable and only it is written.
pub(crate) fn deprecated_constant(req: &mut Request, key: &[u8]) {
    let Some(suffix) = deprecation_suffix(req.constant_attributes(key)) else {
        return;
    };
    let name = String::from_utf8_lossy(key).into_owned();
    deprecated(req, &format!("Constant {name} is deprecated{suffix}"));
}

/// `get_deprecation_suffix_from_attribute`
/// (`Z/Zend/zend_execute.c:1801`): what php prints after `is
/// deprecated`, ` since <since>` and `, <message>`, each written only
/// where the argument of that name is a string; `None` where the list
/// carries no `#[\Deprecated]` at all, which is the raise php does not
/// make.
///
/// php builds the `Deprecated` object to read `$message` and `$since`
/// off it, which sorts the positional arguments by the constructor's
/// order; the class is not registered here yet (#464), so the
/// arguments are read where they were written, by name and, for a
/// positional one, by the position `Deprecated::__construct(?string
/// $message = null, ?string $since = null)` gives it.
pub(crate) fn deprecation_suffix(attributes: &[Attribute]) -> Option<String> {
    let marked = attributes
        .iter()
        .find(|one| one.name.0.eq_ignore_ascii_case(b"Deprecated"))?;
    let mut message = None;
    let mut since = None;
    let mut positional = 0;
    for argument in &marked.arguments {
        let written = match &argument.value {
            ConstExpr::Str(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
            _ => None,
        };
        match argument.name.as_ref().map(|name| &*name.0) {
            Some(b"message") => message = written,
            Some(b"since") => since = written,
            Some(_) => {}
            None => {
                if positional == 0 {
                    message = written;
                } else if positional == 1 {
                    since = written;
                }
                positional += 1;
            }
        }
    }
    let mut suffix = String::new();
    if let Some(since) = since.filter(|s| !s.is_empty()) {
        suffix.push_str(" since ");
        suffix.push_str(&since);
    }
    if let Some(message) = message.filter(|s| !s.is_empty()) {
        suffix.push_str(", ");
        suffix.push_str(&message);
    }
    Some(suffix)
}

/// `php_error_docref(NULL, level, ...)`: a diagnostic from inside a
/// native, its message behind the native's declared name and `(): `,
/// which is how `ext/standard`'s natives raise most of what they raise
/// (`chr(): Providing a value not in-between 0 and 255 is deprecated,
/// ...`). The docref itself, php's link into the manual, is off in the
/// CLI's ini and is not rendered here.
pub fn docref(req: &mut Request, level: Level, message: &str) {
    let text = format!("{}(): {message}", req.active_native());
    raise(req, level, &text);
}

/// `E_ERROR`: `php_error_cb` renders it where the request stands and the
/// request ends after this; the `Thrown` says so.
pub fn fatal(req: &mut Request, message: &str) -> Thrown {
    req.end_in_fatal(message);
    Thrown::of(None)
}

/// Throw a new instance of `class` with `message`, `zend_throw_error`.
/// A throwable the memory limit refuses to build is the fatal that ends
/// the request, already recorded, and the `Thrown` says so.
pub fn throw(req: &mut Request, class: &str, message: &str) -> Thrown {
    match new_throwable(req, class, message) {
        Ok(throwable) => throw_value(req, &throwable),
        Err(fatal) => fatal,
    }
}

/// A throwable of `class` made at a site of its own, the `file` and
/// `line` `zend_default_exception_new` records for a `ParseError` or a
/// `CompileError` while a file is being compiled: the compiled name and
/// `CG(zend_lineno)`, never the executed site. The one thrower here is
/// `token_get_all()` under `TOKEN_PARSE`, whose parse runs under the
/// empty file name, and its witness in `ext/tokenizer` pins the
/// rendering; `a_parse_error_is_rendered_as_its_level` below holds it
/// without the extension.
pub fn throw_at(req: &mut Request, class: &str, message: &str, file: &[u8], line: u32) -> Thrown {
    match throwable_at(req, class, message, file, line) {
        Ok(object) => Thrown::of(Some(object)),
        Err(fatal) => fatal,
    }
}

/// The instance behind [`throw_at`], its strings the request's; the
/// `Err` is the memory limit refusing them.
fn throwable_at(
    req: &mut Request,
    class: &str,
    message: &str,
    file: &[u8],
    line: u32,
) -> Result<PhpObject, Thrown> {
    let object = req.new_object_of(class)?;
    let message = Value::string(PhpString::new(req, message.as_bytes())?);
    let file = Value::string(PhpString::new(req, file)?);
    let line = Value::int(i64::from(line));
    for (name, value) in [(&b"message"[..], message), (b"file", file), (b"line", line)] {
        let name = PhpString::new(req, name)?;
        object.set_prop(req, name, value)?;
    }
    Ok(object)
}

/// Make a throwable pending, `zend_throw_exception_object`: one more
/// owner of the object, the pending slot's, as `ZEND_THROW` takes one
/// before it frees its operand; the caller keeps its own. A value that
/// is not an object becomes an `Error`.
pub fn throw_value(req: &mut Request, throwable: &Value) -> Thrown {
    if let Some(object) = throwable.as_object() {
        req.addref(throwable);
        return Thrown::of(Some(*object));
    }
    throw(
        req,
        "Error",
        "Cannot throw objects that do not implement Throwable",
    )
}

/// A new instance of the throwable class `class` with `message`, not yet
/// pending. `zend_default_exception_new` records where it was made, and
/// that is the site an uncaught one names. Its strings are the
/// request's, and the `Err` is the memory limit refusing them.
pub fn new_throwable(req: &mut Request, class: &str, message: &str) -> Result<Value, Thrown> {
    let object = req.new_object_of(class)?;
    let name = PhpString::new(req, b"message")?;
    let message = PhpString::new(req, message.as_bytes())?;
    object.set_prop(req, name, Value::string(message))?;
    req.note_throw_site(object)?;
    Ok(object.as_value())
}

/// `zend_argument_type_error`: `TypeError` for the argument at
/// `position`, `{fn}(): Argument #N ($name) must {tail}`. The function's
/// name and the parameter's are the active native's declaration, as
/// `get_active_function_arg_name` reads them off `arg_info`, so the
/// caller's spelling of the call is not what the message says and a
/// native never repeats a name it declared. The trace under it prints
/// the argument off the call frame, as php prints what the frame holds.
///
/// ```php
/// Set_Time_Limit('x');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: set_time_limit(): Argument #1 ($seconds) must be of type int, string given in %s:2
/// Stack trace:
/// #0 %s(2): set_time_limit('x')
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub fn arg_type_error(req: &mut Request, position: u32, tail: &str) -> Thrown {
    let message = req.argument_message(position, tail);
    throw(req, "TypeError", &message)
}

/// `zend_argument_value_error`: `ValueError` for the argument at
/// `position`, the same sentence under the other class. No native this
/// engine declares raises one; `ext/standard`'s `str_repeat` does, and
/// its witness pins the rendering. The text is held here by
/// `a_value_error_names_the_parameter` below.
pub fn arg_value_error(req: &mut Request, position: u32, tail: &str) -> Thrown {
    let message = req.argument_message(position, tail);
    throw(req, "ValueError", &message)
}

/// `zend_argument_error`: a throwable of `class` for the argument at
/// `position`, the sentence the two above word, `fn(): Argument #N
/// ($name) <tail>` with the tail as it stands, which is what SPL throws
/// its `OutOfRangeException` for an index with.
///
/// ```php
/// $l = new SplDoublyLinkedList;
/// try { $l->offsetGet(3); } catch (OutOfRangeException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// SplDoublyLinkedList::offsetGet(): Argument #1 ($index) is out of range
/// ```
pub fn argument_error(req: &mut Request, class: &str, position: u32, tail: &str) -> Thrown {
    let message = req.argument_message(position, "");
    let message = format!("{}{tail}", message.trim_end_matches("must "));
    throw(req, class, &message)
}

/// `ArgumentCountError`: `fn() expects <tail>`.
pub fn arg_count_error(req: &mut Request, tail: &str) -> Thrown {
    let message = format!("{}() expects {tail}", req.active_native());
    throw(req, "ArgumentCountError", &message)
}

/// `ArgumentCountError` for a call with fewer arguments than the
/// declaration requires, `zend_wrong_parameters_count_error`.
pub fn too_few_arguments(req: &mut Request, expected: u32, given: u32) -> Thrown {
    let word = if expected == 1 {
        "argument"
    } else {
        "arguments"
    };
    let message = format!(
        "{}() expects at least {expected} {word}, {given} given",
        req.active_native()
    );
    throw(req, "ArgumentCountError", &message)
}

/// `zend_param_must_be_nullable`: the 8.1 deprecation for `null` passed
/// to a non-nullable scalar parameter of a native, one sentence,
/// `{fn}(): Passing null to parameter #N ($name) of type {type} is
/// deprecated`, the name read off the declaration as the type error's
/// is.
///
/// ```php
/// var_dump(ini_set(null, '1'));
/// ```
/// ```expect
/// Deprecated: ini_set(): Passing null to parameter #1 ($option) of type string is deprecated in %s on line 2
/// bool(false)
/// ```
pub fn arg_null_deprecated(req: &mut Request, position: u32, type_name: &str) {
    let text = req.null_parameter_message(position, type_name);
    deprecated(req, &text);
}

/// `ZEND_FUNCTION(exit)` past its parameter parsing
/// (`Z/Zend/zend_builtin_functions.c:118`): a `string` status is
/// written, and one that is not empty may reach a buffer's handler
/// that throws, which is then what the native returns with; an `int` status
/// is php's `EG(exit_status)`, the `int` it is in C, so a code past
/// 32 bits keeps its low ones. Then the unwind exit is thrown
/// ([`Request::throw_unwind_exit`]), which no `catch` matches and no
/// `finally` block is entered for, and the `Thrown` carries no
/// throwable. Every other value is the caller's to coerce or refuse
/// first, as the native `exit` does.
pub fn exit(req: &mut Request, status: &Value) -> Thrown {
    if let Some(text) = status.as_str_bytes() {
        if !text.is_empty() {
            req.write(text);
            if req.is_pending() {
                return Thrown::of(None);
            }
        }
    } else {
        #[allow(clippy::cast_possible_truncation)]
        let code = status.as_int().unwrap_or(0) as i32;
        req.end_in_exit(code);
    }
    req.throw_unwind_exit();
    Thrown::of(None)
}

#[cfg(test)]
mod tests {
    use php_unit::decl::{FunctionDecl, Name, Parameter, Type};

    use crate::api::abi::{Entry, ExtensionBuilder};
    use crate::api::request::Request;
    use crate::api::values::Value;
    use crate::engine::Engine;
    use crate::test::Capture;

    use super::{Thrown, arg_type_error, arg_value_error, throw_at};

    fn parameter(name: &[u8], ty: Type, variadic: bool) -> Parameter {
        Parameter {
            name: Name(name.into()),
            ty: Some(ty),
            by_ref: false,
            variadic,
            default: None,
            attributes: Vec::new(),
        }
    }

    fn declared(name: &[u8], parameters: Vec<Parameter>) -> FunctionDecl {
        FunctionDecl {
            name: Name(name.into()),
            parameters,
            return_type: None,
            returns_by_ref: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
            frameless: Vec::new(),
        }
    }

    /// `str_repeat(string $string, int $times)` as far as its one
    /// `ValueError` goes.
    fn str_repeat(req: &mut Request) -> Result<Value, Thrown> {
        Err(arg_value_error(req, 2, "be greater than or equal to 0"))
    }

    /// `array_merge(array ...$arrays)` as far as its `TypeError` goes.
    fn array_merge(req: &mut Request) -> Result<Value, Thrown> {
        Err(arg_type_error(req, 1, "be of type array, int given"))
    }

    /// What `token_get_all("<?php if (", TOKEN_PARSE)` throws: a
    /// `ParseError` at the compiled site, the empty file name and the
    /// scanner's line.
    fn refuse(req: &mut Request) -> Result<Value, Thrown> {
        Err(throw_at(req, "ParseError", "Unclosed '('", b"", 1))
    }

    /// The fatal a source comes to under an engine with the natives
    /// above declared beside the core's.
    fn fatal_of(source: &str) -> Option<String> {
        served(source).fatal
    }

    fn served(source: &str) -> crate::test::Served {
        let extension = ExtensionBuilder::default()
            .functions(vec![
                (
                    declared(
                        b"str_repeat",
                        vec![
                            parameter(b"string", Type::String, false),
                            parameter(b"times", Type::Int, false),
                        ],
                    ),
                    str_repeat as Entry,
                ),
                (
                    declared(
                        b"array_merge",
                        vec![parameter(b"arrays", Type::Array, true)],
                    ),
                    array_merge as Entry,
                ),
                (declared(b"refuse", Vec::new()), refuse as Entry),
            ])
            .build("raising");
        Engine::builder()
            .extensions(&[extension])
            .build()
            .serve_capture(source, Capture::default())
    }

    /// `zend_argument_type_error` under the pinned oracle, the name off
    /// the declaration whatever case the call spelled:
    ///
    /// ```text
    /// $ php -n -r 'Set_Time_Limit("x");'
    /// Fatal error: Uncaught TypeError: set_time_limit(): Argument #1 ($seconds) must be of type int, string given in Command line code:1
    /// ```
    #[test]
    fn a_type_error_names_the_parameter_off_the_declaration() {
        let fatal = fatal_of("<?php Set_Time_Limit('x');");
        assert_eq!(
            fatal.as_deref().map(|f| f.contains(
                "TypeError: set_time_limit(): Argument #1 ($seconds) must be of type int, string given"
            )),
            Some(true),
            "{fatal:?}"
        );
    }

    /// `zend_argument_value_error` under the pinned oracle:
    ///
    /// ```text
    /// $ php -n -r 'str_repeat("x", -1);'
    /// Fatal error: Uncaught ValueError: str_repeat(): Argument #2 ($times) must be greater than or equal to 0 in Command line code:1
    /// ```
    #[test]
    fn a_value_error_names_the_parameter() {
        let fatal = fatal_of("<?php str_repeat('x', -1);");
        assert_eq!(
            fatal.as_deref().map(|f| f.contains(
                "ValueError: str_repeat(): Argument #2 ($times) must be greater than or equal to 0"
            )),
            Some(true),
            "{fatal:?}"
        );
    }

    /// `get_function_arg_name` answers no name past `num_args`, which is
    /// where php keeps the variadic's `arg_info`, so an argument at a
    /// variadic slot is `Argument #N` alone. The pinned oracle:
    ///
    /// ```text
    /// $ php -n -r 'array_merge(1);'
    /// Fatal error: Uncaught TypeError: array_merge(): Argument #1 must be of type array, int given in Command line code:1
    /// ```
    #[test]
    fn an_argument_at_a_variadic_slot_has_no_name() {
        let fatal = fatal_of("<?php array_merge(1);");
        assert_eq!(
            fatal.as_deref().map(|f| f.contains(
                "TypeError: array_merge(): Argument #1 must be of type array, int given"
            )),
            Some(true),
            "{fatal:?}"
        );
    }

    /// `zend_exception_error` on an uncaught `ParseError`: the message as
    /// `E_PARSE` at the throwable's own site, no `Uncaught` and no trace,
    /// and the request ends at 255 with nothing after it. The pinned
    /// oracle:
    ///
    /// ```text
    /// $ php -n -r 'token_get_all("<?php if (", TOKEN_PARSE); echo "after";'
    /// Parse error: Unclosed '(' in  on line 1
    /// ```
    #[test]
    fn a_parse_error_is_rendered_as_its_level() {
        let served = served("<?php refuse(); echo 'after';");
        assert_eq!(
            String::from_utf8_lossy(&served.stdout),
            "\nParse error: Unclosed '(' in  on line 1\n"
        );
        assert_eq!(served.exit_code, 255);
        assert_eq!(served.fatal, None);
    }
}
