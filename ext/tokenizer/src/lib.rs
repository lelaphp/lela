//! The `tokenizer` extension: `php-src/ext/tokenizer` as one crate under
//! `ext/` (ADR-0004). One dependency, the facade; one entry, `extension()`;
//! its state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/tokenizer/tests`, through `xtask phpt --subset`.
//!
//! Two natives, `token_get_all` and `token_name`, over the one lexer
//! (design §4.9) through `php_ext::syntax` and the `T_*` constants of
//! [`names`]. What php's scanner cuts differently from the lexer is put
//! back in [`tokens`]; the names the lexer's kinds carry come from
//! `php_names` and are not repeated here. Under `TOKEN_PARSE` the one
//! parser runs over the same stream and what it took as a name is read
//! off the tree in [`feedback`]. `PhpToken` is the one native class,
//! declared through `ExtensionBuilder::classes` with its methods'
//! entries beside it (design §7.1): `tokenize` over the same two paths,
//! its objects filled as `add_token` fills them, and the instance
//! methods over the four typed properties.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod feedback;
mod names;
mod tokens;

use std::collections::HashMap;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::call::new_object;
use php_ext::coerce::coerce_param;
use php_ext::decl::{
    ClassDecl, ClassFlags, ClassKind, ConstDecl, ConstExpr, EnumBacking, FunctionDecl, MethodDecl,
    Name, Parameter, PropertyDecl, Type, Visibility,
};
use php_ext::raising::{
    Level, Thrown, arg_count_error, arg_type_error, raise, raise_at, throw, throw_at,
};
use php_ext::request::Request;
use php_ext::syntax::{self, FileId, LexOptions};
use php_ext::values::{PhpArray, PhpObject, PhpString, Value, ValueRef};

/// The one bit of `$flags` that `token_get_all` reads.
const TOKEN_PARSE: i64 = 1;

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {}

/// The extension value (design §7.1): declarations paired with entries,
/// the `T_*` constants and `TOKEN_PARSE`, and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('tokenizer'), TOKEN_PARSE, T_LNUMBER, T_DOUBLE_COLON, T_PAAMAYIM_NEKUDOTAYIM, T_BAD_CHARACTER);
/// ```
/// ```expect
/// bool(true)
/// int(1)
/// int(260)
/// int(402)
/// int(402)
/// int(411)
/// ```
pub fn extension() -> Extension {
    // `tokenizer_data.stub.php`'s order, which `php --re tokenizer`
    // prints: the `T_*` by number with `T_PAAMAYIM_NEKUDOTAYIM` at 402's
    // place and `T_DOUBLE_COLON` last, then `TOKEN_PARSE` from
    // `tokenizer.stub.php`, registered after them.
    let mut constants = Vec::new();
    for (name, id) in names::TOKENS {
        constants.push(constant(name.as_bytes(), id));
        if name == "T_CURLY_OPEN"
            && let Some(id) = names::id_of("T_DOUBLE_COLON")
        {
            constants.push(constant(b"T_PAAMAYIM_NEKUDOTAYIM", id));
        }
    }
    constants.push(constant(b"TOKEN_PARSE", TOKEN_PARSE));
    ExtensionBuilder::default()
        .functions(vec![
            (
                declared(
                    b"token_get_all",
                    &[
                        parameter(b"code", Type::String),
                        Parameter {
                            default: Some(ConstExpr::Long(0)),
                            ..parameter(b"flags", Type::Int)
                        },
                    ],
                    Type::Array,
                ),
                token_get_all,
            ),
            (
                declared(b"token_name", &[parameter(b"id", Type::Int)], Type::String),
                token_name,
            ),
        ])
        .classes(vec![php_token_class()])
        .constants(constants)
        .request_state::<State>()
        .info(tokenizer_info)
        .build("tokenizer")
}

/// `PHP_MINFO_FUNCTION(tokenizer)`: the one-row table `Tokenizer
/// Support => enabled`, what `phpinfo()`, `php --ri tokenizer` and
/// `ReflectionExtension::info` print for the extension.
///
/// ```php
/// (new ReflectionExtension('tokenizer'))->info();
/// ```
/// ```expect
/// tokenizer
///
/// Tokenizer Support => enabled
/// ```
fn tokenizer_info(req: &mut Request) {
    req.info_table(&[&["Tokenizer Support", "enabled"]]);
}

/// `PhpToken` as `register_class_PhpToken` declares it from
/// `tokenizer.stub.php`: a class implementing `Stringable`, the four
/// public typed properties `id`, `text`, `line` and `pos` with no
/// default, in that order, since the methods read them by slot as
/// `OBJ_PROP_NUM` reads them, and the six methods, each declaration
/// beside its entry. `tokenize` is the one called on the class;
/// `__construct` is final.
fn php_token_class() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    let property = |name: &[u8], ty: Type| PropertyDecl {
        name: Name(name.into()),
        visibility: Visibility::Public,
        set_visibility: None,
        is_instance: true,
        is_readonly: false,
        is_final: false,
        ty: Some(ty),
        default: None,
        attributes: Vec::new(),
        doc_comment: None,
    };
    let decl = ClassDecl {
        name: Name(b"PhpToken".as_slice().into()),
        kind: ClassKind::Class,
        flags: ClassFlags::default(),
        parent: None,
        interfaces: vec![Name(b"Stringable".as_slice().into())],
        traits: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        properties: vec![
            property(b"id", Type::Int),
            property(b"text", Type::String),
            property(b"line", Type::Int),
            property(b"pos", Type::Int),
        ],
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    };
    (decl, php_token_methods())
}

/// The six methods of `PhpToken`, each declaration beside its entry.
fn php_token_methods() -> Vec<(MethodDecl, Entry)> {
    let minus_one = |name: &[u8]| Parameter {
        default: Some(ConstExpr::Long(-1)),
        ..parameter(name, Type::Int)
    };
    vec![
        (
            method(
                declared(
                    b"tokenize",
                    &[
                        parameter(b"code", Type::String),
                        Parameter {
                            default: Some(ConstExpr::Long(0)),
                            ..parameter(b"flags", Type::Int)
                        },
                    ],
                    Type::Array,
                ),
                false,
                false,
            ),
            token_tokenize as Entry,
        ),
        (
            method(
                FunctionDecl {
                    return_type: None,
                    ..declared(
                        b"__construct",
                        &[
                            parameter(b"id", Type::Int),
                            parameter(b"text", Type::String),
                            minus_one(b"line"),
                            minus_one(b"pos"),
                        ],
                        Type::Void,
                    )
                },
                true,
                true,
            ),
            token_construct as Entry,
        ),
        (
            method(
                declared(
                    b"is",
                    &[Parameter {
                        ty: None,
                        ..parameter(b"kind", Type::Int)
                    }],
                    Type::Bool,
                ),
                true,
                false,
            ),
            token_is as Entry,
        ),
        (
            method(declared(b"isIgnorable", &[], Type::Bool), true, false),
            token_is_ignorable as Entry,
        ),
        (
            method(
                declared(b"getTokenName", &[], Type::Nullable(Box::new(Type::String))),
                true,
                false,
            ),
            token_get_token_name as Entry,
        ),
        (
            method(declared(b"__toString", &[], Type::String), true, false),
            token_to_string as Entry,
        ),
    ]
}

/// The entry an extension pairs with a declaration, as `functions`
/// takes it.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// One public method's declaration over a function's.
fn method(function: FunctionDecl, is_instance: bool, is_final: bool) -> MethodDecl {
    MethodDecl {
        function,
        visibility: Visibility::Public,
        is_instance,
        is_abstract: false,
        is_final,
        tentative_return: false,
    }
}

/// `PhpToken::tokenize(string $code, int $flags = 0): array`, php's
/// `PHP_METHOD(PhpToken, tokenize)` (`Z/ext/tokenizer/tokenizer.c:90`):
/// `tokenize_common` with the called class as `token_class`, so
/// `add_token` lays each token down as an object of it, `id`, `text`,
/// `line` and `pos` written by slot, `pos` the byte the token starts at
/// (`text - LANG_SCNG(yy_start)`); a single character is its own byte
/// as `id`. The two paths are [`token_get_all`]'s.
///
/// ```php
/// var_dump(PhpToken::tokenize('<?php echo 1;'));
/// ```
/// ```expect
/// array(5) {
///   [0]=>
///   object(PhpToken)#1 (4) {
///     ["id"]=>
///     int(394)
///     ["text"]=>
///     string(6) "<?php "
///     ["line"]=>
///     int(1)
///     ["pos"]=>
///     int(0)
///   }
///   [1]=>
///   object(PhpToken)#2 (4) {
///     ["id"]=>
///     int(291)
///     ["text"]=>
///     string(4) "echo"
///     ["line"]=>
///     int(1)
///     ["pos"]=>
///     int(6)
///   }
///   [2]=>
///   object(PhpToken)#3 (4) {
///     ["id"]=>
///     int(397)
///     ["text"]=>
///     string(1) " "
///     ["line"]=>
///     int(1)
///     ["pos"]=>
///     int(10)
///   }
///   [3]=>
///   object(PhpToken)#4 (4) {
///     ["id"]=>
///     int(260)
///     ["text"]=>
///     string(1) "1"
///     ["line"]=>
///     int(1)
///     ["pos"]=>
///     int(11)
///   }
///   [4]=>
///   object(PhpToken)#5 (4) {
///     ["id"]=>
///     int(59)
///     ["text"]=>
///     string(1) ";"
///     ["line"]=>
///     int(1)
///     ["pos"]=>
///     int(12)
///   }
/// }
/// ```
///
/// The objects of one call given up are the store's free list, taken
/// last freed first (`zend_objects_store_put`), so the next call's
/// first token is the id the last of them had; and `TOKEN_PARSE` runs
/// the parser, as it does under `token_get_all`.
///
/// ```php
/// var_dump(count(PhpToken::tokenize("<?php \$x->class;", TOKEN_PARSE)), PhpToken::tokenize("<?php ", 0));
/// ```
/// ```expect
/// int(5)
/// array(1) {
///   [0]=>
///   object(PhpToken)#5 (4) {
///     ["id"]=>
///     int(394)
///     ["text"]=>
///     string(6) "<?php "
///     ["line"]=>
///     int(1)
///     ["pos"]=>
///     int(0)
///   }
/// }
/// ```
///
/// The texts are `make_str`'s here as under `token_get_all` ([`Texts`]):
/// the second `echo` and the second `1;` are one more owner each of the
/// first's string, and no string of their own.
///
/// ```php
/// $b = memory_get_usage();
/// $t = PhpToken::tokenize('<?php echo 1; echo 2;'); $held = memory_get_usage() - $b;
/// $t = 1; var_dump($held, memory_get_usage() - $b);
/// ```
/// ```expect
/// int(1560)
/// int(0)
/// ```
fn token_tokenize(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if !(1..=2).contains(&count) {
        return Err(wrong_argument_count(req, 1, 2, count));
    }
    let code = string_argument(req, 1)?;
    let flags = if count == 2 {
        long_argument(req, 2)?
    } else {
        0
    };
    let Some(token_class) = req.called_scope() else {
        return Err(throw(
            req,
            "Error",
            "PhpToken::tokenize() called with no class",
        ));
    };
    let short_open_tag = req
        .ini_get(b"short_open_tag")
        .is_none_or(Request::ini_parse_bool);
    let tokens = if flags & TOKEN_PARSE == 0 {
        tokenize(req, code.as_bytes(), short_open_tag)
    } else {
        tokenize_parse(req, code.as_bytes(), short_open_tag)?
    };
    let mut texts = Texts::for_flags(flags);
    let mut rows = PhpArray::new(req)?;
    for token in tokens {
        let text = code.as_bytes().get(token.start..token.end).unwrap_or(&[]);
        let id = match token.name.and_then(names::id_of) {
            Some(id) => id,
            None => i64::from(text.first().copied().unwrap_or(0)),
        };
        let object = new_object(req, token_class)?;
        let text = texts.make_str(req, text)?;
        let pos = i64::try_from(token.start).unwrap_or(i64::MAX);
        fill_token(req, object, id, text, i64::from(token.line), pos)?;
        rows.append(req, Value::object(object))?;
    }
    Ok(Value::array(rows))
}

/// `ZVAL_LONG(OBJ_PROP_NUM(obj, 0), ...)` and the three after it: the
/// four properties written by slot, no name in hand, `text` the
/// object's from here. What a token costs is what php's costs: the
/// object and its slots, and the text where [`Texts`] has no owner of
/// it yet, which the peak reads.
///
/// ```php
/// $b = memory_get_peak_usage(); PhpToken::tokenize('<?php echo 1; echo 2;'); var_dump(memory_get_peak_usage() - $b);
/// ```
/// ```expect
/// int(1560)
/// ```
fn fill_token(
    req: &mut Request,
    object: PhpObject,
    id: i64,
    text: Value,
    line: i64,
    pos: i64,
) -> Result<(), Thrown> {
    object.set_slot(req, 0, Value::int(id))?;
    object.set_slot(req, 1, text)?;
    object.set_slot(req, 2, Value::int(line))?;
    object.set_slot(req, 3, Value::int(pos))
}

/// `PhpToken::__construct(int $id, string $text, int $line = -1, int $pos = -1)`,
/// php's `PHP_METHOD(PhpToken, __construct)` (`Z/ext/tokenizer/tokenizer.c:118`):
/// the four properties written by slot, the two without an argument
/// `-1`.
///
/// ```php
/// var_dump(new PhpToken(T_ECHO, "echo"), new PhpToken(59, ";", 3, 7));
/// ```
/// ```expect
/// object(PhpToken)#1 (4) {
///   ["id"]=>
///   int(291)
///   ["text"]=>
///   string(4) "echo"
///   ["line"]=>
///   int(-1)
///   ["pos"]=>
///   int(-1)
/// }
/// object(PhpToken)#2 (4) {
///   ["id"]=>
///   int(59)
///   ["text"]=>
///   string(1) ";"
///   ["line"]=>
///   int(3)
///   ["pos"]=>
///   int(7)
/// }
/// ```
///
/// `ZEND_PARSE_PARAMETERS_START(2, 4)`: a call outside that arity is
/// php's `ArgumentCountError` under the method's qualified name, and
/// the trace names the constructor on its receiver.
///
/// ```php
/// new PhpToken("a");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: PhpToken::__construct() expects at least 2 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): PhpToken->__construct('a')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn token_construct(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if !(2..=4).contains(&count) {
        return Err(wrong_argument_count(req, 2, 4, count));
    }
    let this = receiver(req)?;
    let id = long_argument(req, 1)?;
    let text = Value::string(string_argument(req, 2)?);
    let line = if count >= 3 {
        long_argument(req, 3)?
    } else {
        -1
    };
    let pos = if count == 4 {
        long_argument(req, 4)?
    } else {
        -1
    };
    // `ZVAL_STR_COPY`: the argument keeps its owner and the property
    // takes one of its own, once every argument has been read.
    req.addref(&text);
    fill_token(req, this, id, text, line, pos)?;
    Ok(Value::null())
}

/// `PhpToken::is(int|string|array $kind): bool`, php's
/// `PHP_METHOD(PhpToken, is)` (`Z/ext/tokenizer/tokenizer.c:141`): an
/// `int` against `id`, a string against `text`, an array against each
/// of its elements in turn, `true` at the first that matches, and
/// php's `TypeError` for anything else, or for an element that is
/// neither.
///
/// ```php
/// $t = new PhpToken(T_ECHO, "echo"); var_dump($t->is(T_ECHO), $t->is("echo"), $t->is(1), $t->is("x"));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// $t = new PhpToken(1, "x");
/// $t->is(1.5);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: PhpToken::is(): Argument #1 ($kind) must be of type string|int|array, float given in %s:3
/// Stack trace:
/// #0 %s(3): PhpToken->is(1.5)
/// #1 {main}
///   thrown in %s on line 3
/// ```
fn token_is(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let this = receiver(req)?;
    let kind = req.argument(1).unwrap_or_default().deref();
    let answer = match kind.as_ref() {
        ValueRef::Int(n) => token_id(req, this)? == n,
        ValueRef::Str(s) => token_text(req, this)?.as_bytes() == s.as_bytes(),
        ValueRef::Array(kinds) => {
            let mut id = None;
            let mut text = None;
            let mut found = false;
            for (_, entry) in kinds.snapshot() {
                let entry = entry.deref();
                match entry.as_ref() {
                    ValueRef::Int(n) => {
                        let id = match id {
                            Some(id) => id,
                            None => *id.insert(token_id(req, this)?),
                        };
                        if id == n {
                            found = true;
                            break;
                        }
                    }
                    ValueRef::Str(s) => {
                        let text = match text {
                            Some(text) => text,
                            None => *text.insert(token_text(req, this)?),
                        };
                        if text.as_bytes() == s.as_bytes() {
                            found = true;
                            break;
                        }
                    }
                    _ => {
                        return Err(arg_type_error(
                            req,
                            1,
                            &format!(
                                "only have elements of type string|int, {} given",
                                value_name(req, &entry)
                            ),
                        ));
                    }
                }
            }
            found
        }
        _ => {
            return Err(arg_type_error(
                req,
                1,
                &format!(
                    "be of type string|int|array, {} given",
                    value_name(req, &kind)
                ),
            ));
        }
    };
    Ok(Value::bool(answer))
}

/// `PhpToken::isIgnorable(): bool`, php's `PHP_METHOD(PhpToken, isIgnorable)`
/// (`Z/ext/tokenizer/tokenizer.c:200`): whether `id` is `T_WHITESPACE`,
/// `T_COMMENT`, `T_DOC_COMMENT` or `T_OPEN_TAG`.
///
/// ```php
/// var_dump((new PhpToken(T_WHITESPACE, " "))->isIgnorable(), (new PhpToken(T_DOC_COMMENT, "/** */"))->isIgnorable(), (new PhpToken(T_ECHO, "echo"))->isIgnorable());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
fn token_is_ignorable(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 0 {
        return Err(wrong_argument_count(req, 0, 0, count));
    }
    let this = receiver(req)?;
    let id = token_id(req, this)?;
    let ignorable = ["T_WHITESPACE", "T_COMMENT", "T_DOC_COMMENT", "T_OPEN_TAG"]
        .iter()
        .filter_map(|name| names::id_of(name))
        .any(|ignorable| ignorable == id);
    Ok(Value::bool(ignorable))
}

/// `PhpToken::getTokenName(): ?string`, php's
/// `PHP_METHOD(PhpToken, getTokenName)` (`Z/ext/tokenizer/tokenizer.c:213`):
/// an `id` below 256 is the one-byte string of that byte (`RETURN_CHAR`),
/// a token's number its `T_*` name, and a number no token has `null`.
///
/// ```php
/// $t = new PhpToken(T_ECHO, "echo"); $u = new PhpToken(59, ";", 3, 7);
/// var_dump($t->getTokenName(), $u->getTokenName(), (new PhpToken(9999, "x"))->getTokenName());
/// ```
/// ```expect
/// string(6) "T_ECHO"
/// string(1) ";"
/// NULL
/// ```
///
/// `ZEND_PARSE_PARAMETERS_NONE()`: an argument is php's
/// `ArgumentCountError`, and the trace names the method on its receiver.
///
/// ```php
/// $t = new PhpToken(T_ECHO, "echo");
/// $t->getTokenName(1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: PhpToken::getTokenName() expects exactly 0 arguments, 1 given in %s:3
/// Stack trace:
/// #0 %s(3): PhpToken->getTokenName(1)
/// #1 {main}
///   thrown in %s on line 3
/// ```
///
/// An instance method called on the class is `zend_non_static_method_call`.
///
/// ```php
/// PhpToken::getTokenName();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Non-static method PhpToken::getTokenName() cannot be called statically in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
fn token_get_token_name(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 0 {
        return Err(wrong_argument_count(req, 0, 0, count));
    }
    let this = receiver(req)?;
    let id = token_id(req, this)?;
    if id < 256 {
        let byte = u8::try_from(id.rem_euclid(256)).unwrap_or(0);
        return Ok(Value::string(PhpString::new(req, &[byte])?));
    }
    match names::name_of(id) {
        Some(name) => Ok(Value::string(PhpString::new(req, name.as_bytes())?)),
        None => Ok(Value::null()),
    }
}

/// `PhpToken::__toString(): string`, php's `PHP_METHOD(PhpToken, __toString)`
/// (`Z/ext/tokenizer/tokenizer.c:234`): `text`, with an owner of its own,
/// `RETURN_STR_COPY`, since the property keeps the one it has.
///
/// ```php
/// var_dump((new PhpToken(T_ECHO, "echo"))->__toString());
/// ```
/// ```expect
/// string(4) "echo"
/// ```
///
/// So the text outlives every call that returned it: two calls give
/// two owners up, and the property's string is still the token's.
///
/// ```php
/// $t = new PhpToken(T_ECHO, str_repeat("echo", 8));
/// var_dump($t->__toString(), $t->__toString());
/// $pad = str_repeat("-", 32);
/// var_dump($t->text);
/// ```
/// ```expect
/// string(32) "echoechoechoechoechoechoechoecho"
/// string(32) "echoechoechoechoechoechoechoecho"
/// string(32) "echoechoechoechoechoechoechoecho"
/// ```
fn token_to_string(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 0 {
        return Err(wrong_argument_count(req, 0, 0, count));
    }
    let this = receiver(req)?;
    let text = Value::string(token_text(req, this)?);
    req.addref(&text);
    Ok(text)
}

/// `ZEND_THIS` of a method being served.
fn receiver(req: &mut Request) -> Result<PhpObject, Thrown> {
    req.this()
        .ok_or_else(|| throw(req, "Error", "PhpToken method called with no receiver"))
}

/// `php_token_get_id`: `id` off the receiver, and php's `Error` for a
/// typed property nothing has written.
fn token_id(req: &mut Request, this: PhpObject) -> Result<i64, Thrown> {
    match this.prop(req, b"id").and_then(|id| id.deref().as_int()) {
        Some(id) => Ok(id),
        None => Err(throw(
            req,
            "Error",
            "Typed property PhpToken::$id must not be accessed before initialization",
        )),
    }
}

/// `php_token_get_text`: `text` off the receiver, a view of it, and
/// php's `Error` for a typed property nothing has written.
fn token_text(req: &mut Request, this: PhpObject) -> Result<PhpString, Thrown> {
    match this
        .prop(req, b"text")
        .and_then(|text| text.deref().as_php_string().copied())
    {
        Some(text) => Ok(text),
        None => Err(throw(
            req,
            "Error",
            "Typed property PhpToken::$text must not be accessed before initialization",
        )),
    }
}

/// `zend_zval_value_name`: what a `TypeError` names a value as, its
/// class for an object and `true` or `false` for a bool.
fn value_name(req: &Request, value: &Value) -> String {
    match value.as_ref() {
        ValueRef::Object(o) => req.class_decl(o.class()).map_or_else(
            || String::from("object"),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        ),
        ValueRef::Bool(b) => String::from(if b { "true" } else { "false" }),
        _ => String::from(value.type_name()),
    }
}

/// `token_get_all(string $code, int $flags = 0): array`, php's
/// `tokenize()` (`Z/ext/tokenizer/tokenizer.c:321`): the scanner over
/// `$code` from `INITIAL`, and one row per token, `[id, text,
/// line]` for a token with a `T_*` number and the character itself for
/// one without.
///
/// ```php
/// var_dump(token_get_all('<?php echo 1;'));
/// ```
/// ```expect
/// array(5) {
///   [0]=>
///   array(3) {
///     [0]=>
///     int(394)
///     [1]=>
///     string(6) "<?php "
///     [2]=>
///     int(1)
///   }
///   [1]=>
///   array(3) {
///     [0]=>
///     int(291)
///     [1]=>
///     string(4) "echo"
///     [2]=>
///     int(1)
///   }
///   [2]=>
///   array(3) {
///     [0]=>
///     int(397)
///     [1]=>
///     string(1) " "
///     [2]=>
///     int(1)
///   }
///   [3]=>
///   array(3) {
///     [0]=>
///     int(260)
///     [1]=>
///     string(1) "1"
///     [2]=>
///     int(1)
///   }
///   [4]=>
///   string(1) ";"
/// }
/// ```
///
/// A heredoc comes back as php's scanner cuts it, the indentation the
/// lexer records as `HEREDOC_INDENT` put back into the body text and
/// onto the closing marker, and a line is the one the token starts on.
/// `HANDLE_NEWLINES` counts a `\r\n` once and a lone `\r` too, which
/// `a_line_is_counted_as_handle_newlines_counts` in `tests/capture.rs`
/// holds, since the bytes it takes are ones a fence cannot carry.
///
/// ```php
/// var_dump(token_get_all("<?php <<<E\n  \$a\n  E;"));
/// ```
/// ```expect
/// array(7) {
///   [0]=>
///   array(3) {
///     [0]=>
///     int(394)
///     [1]=>
///     string(6) "<?php "
///     [2]=>
///     int(1)
///   }
///   [1]=>
///   array(3) {
///     [0]=>
///     int(398)
///     [1]=>
///     string(5) "<<<E
/// "
///     [2]=>
///     int(1)
///   }
///   [2]=>
///   array(3) {
///     [0]=>
///     int(268)
///     [1]=>
///     string(2) "  "
///     [2]=>
///     int(2)
///   }
///   [3]=>
///   array(3) {
///     [0]=>
///     int(266)
///     [1]=>
///     string(2) "$a"
///     [2]=>
///     int(2)
///   }
///   [4]=>
///   array(3) {
///     [0]=>
///     int(268)
///     [1]=>
///     string(1) "
/// "
///     [2]=>
///     int(2)
///   }
///   [5]=>
///   array(3) {
///     [0]=>
///     int(399)
///     [1]=>
///     string(3) "  E"
///     [2]=>
///     int(3)
///   }
///   [6]=>
///   string(1) ";"
/// }
/// ```
///
/// What the lexer leaves to this extension: `private(set)` is one token
/// of php's, `yield from` one across the whitespace between, `enum` a
/// keyword only before a name, `callable` a keyword the lexer does not
/// know, a double-quoted string nothing interpolates in one
/// `T_CONSTANT_ENCAPSED_STRING`, and `${x}` opens with
/// `T_DOLLAR_OPEN_CURLY_BRACES` and names `T_STRING_VARNAME`.
///
/// ```php
/// var_dump(token_get_all('<?php private(set);yield from;enum;enum X;callable;"a";"${x}"'));
/// ```
/// ```expect
/// array(20) {
///   [0]=>
///   array(3) {
///     [0]=>
///     int(394)
///     [1]=>
///     string(6) "<?php "
///     [2]=>
///     int(1)
///   }
///   [1]=>
///   array(3) {
///     [0]=>
///     int(327)
///     [1]=>
///     string(12) "private(set)"
///     [2]=>
///     int(1)
///   }
///   [2]=>
///   string(1) ";"
///   [3]=>
///   array(3) {
///     [0]=>
///     int(282)
///     [1]=>
///     string(10) "yield from"
///     [2]=>
///     int(1)
///   }
///   [4]=>
///   string(1) ";"
///   [5]=>
///   array(3) {
///     [0]=>
///     int(262)
///     [1]=>
///     string(4) "enum"
///     [2]=>
///     int(1)
///   }
///   [6]=>
///   string(1) ";"
///   [7]=>
///   array(3) {
///     [0]=>
///     int(339)
///     [1]=>
///     string(4) "enum"
///     [2]=>
///     int(1)
///   }
///   [8]=>
///   array(3) {
///     [0]=>
///     int(397)
///     [1]=>
///     string(1) " "
///     [2]=>
///     int(1)
///   }
///   [9]=>
///   array(3) {
///     [0]=>
///     int(262)
///     [1]=>
///     string(1) "X"
///     [2]=>
///     int(1)
///   }
///   [10]=>
///   string(1) ";"
///   [11]=>
///   array(3) {
///     [0]=>
///     int(345)
///     [1]=>
///     string(8) "callable"
///     [2]=>
///     int(1)
///   }
///   [12]=>
///   string(1) ";"
///   [13]=>
///   array(3) {
///     [0]=>
///     int(269)
///     [1]=>
///     string(3) ""a""
///     [2]=>
///     int(1)
///   }
///   [14]=>
///   string(1) ";"
///   [15]=>
///   string(1) """
///   [16]=>
///   array(3) {
///     [0]=>
///     int(400)
///     [1]=>
///     string(2) "${"
///     [2]=>
///     int(1)
///   }
///   [17]=>
///   array(3) {
///     [0]=>
///     int(270)
///     [1]=>
///     string(1) "x"
///     [2]=>
///     int(1)
///   }
///   [18]=>
///   string(1) "}"
///   [19]=>
///   string(1) """
/// }
/// ```
///
/// A byte that pops `ST_VAR_OFFSET` (`<ST_VAR_OFFSET>[ \n\r\t\\'#]`,
/// `yyless(0)` then a zero-width `T_ENCAPSED_AND_WHITESPACE`) is an
/// empty row between the `[` and the text that starts at the same byte:
/// the lexer carries the token, and this walk keeps it out of the
/// stretch of text after it.
///
/// ```php
/// var_dump(token_get_all('<?php "$a[ 1 ]"'));
/// ```
/// ```expect exit=0
/// array(7) {
///   [0]=>
///   array(3) {
///     [0]=>
///     int(394)
///     [1]=>
///     string(6) "<?php "
///     [2]=>
///     int(1)
///   }
///   [1]=>
///   string(1) """
///   [2]=>
///   array(3) {
///     [0]=>
///     int(266)
///     [1]=>
///     string(2) "$a"
///     [2]=>
///     int(1)
///   }
///   [3]=>
///   string(1) "["
///   [4]=>
///   array(3) {
///     [0]=>
///     int(268)
///     [1]=>
///     string(0) ""
///     [2]=>
///     int(1)
///   }
///   [5]=>
///   array(3) {
///     [0]=>
///     int(268)
///     [1]=>
///     string(4) " 1 ]"
///     [2]=>
///     int(1)
///   }
///   [6]=>
///   string(1) """
/// }
/// ```
///
/// `ZEND_PARSE_PARAMETERS_START(1, 2)`: `Z_PARAM_STR` over the code and
/// an optional `Z_PARAM_LONG` over the flags, so `1` is the code `"1"`,
/// and a call outside that arity is php's `ArgumentCountError`, which
/// `the_arity_is_php_s` in `tests/capture.rs` holds without the oracle.
/// `flags & TOKEN_PARSE` chooses between [`tokenize`] and
/// [`tokenize_parse`], as `tokenize_common` chooses.
///
/// **What the rows take is what php's take** (design §5.3, §5.13). The
/// rows and the strings in them are the request's heap's: `array_init`
/// is the 56-byte header, the first `add_next_index` a packed table of
/// eight, 136 bytes in the 160-byte bin, and `array_init_size(&token,
/// 3)` the same two allocations per row, since `zend_hash_check_size`
/// rounds three up to eight; a token's text is `zend_string_init` past
/// one byte and `ZSTR_CHAR` at one (`make_str`), which allocates
/// nothing. Four rows of three and the table of five, with three texts
/// past one byte: `216 + 4 * 216 + 3 * 32`, read before anything is
/// printed because php's first write holds thirty-two bytes for its
/// `php_output` layer, and all of it back when the variable gives the
/// array up. A text that repeats within one call is one string with one
/// more owner per repeat (`make_str`'s `interned_strings`, [`Texts`]):
/// the second `echo` and the second `1;` below add their rows and no
/// string, where `TOKEN_PARSE`, which hands `make_str` no table, adds a
/// string for the second `echo` as well.
///
/// ```php
/// $b = memory_get_usage();
/// $t = token_get_all('<?php echo 12;'); $held = memory_get_usage() - $b;
/// $t = 1; var_dump($held, memory_get_usage() - $b);
/// $b = memory_get_usage();
/// $t = token_get_all('<?php echo 1; echo 2;'); $held = memory_get_usage() - $b;
/// $t = 1; var_dump($held, memory_get_usage() - $b);
/// $b = memory_get_usage();
/// $t = token_get_all('<?php echo 1; echo 2;', TOKEN_PARSE); $held = memory_get_usage() - $b;
/// $t = 1; var_dump($held, memory_get_usage() - $b);
/// ```
/// ```expect
/// int(1176)
/// int(0)
/// int(2168)
/// int(0)
/// int(2200)
/// int(0)
/// ```
///
/// ```php
/// var_dump(token_get_all(1));
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   array(3) {
///     [0]=>
///     int(267)
///     [1]=>
///     string(1) "1"
///     [2]=>
///     int(1)
///   }
/// }
/// ```
fn token_get_all(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if !(1..=2).contains(&count) {
        return Err(wrong_argument_count(req, 1, 2, count));
    }
    let code = string_argument(req, 1)?;
    let flags = if count == 2 {
        long_argument(req, 2)?
    } else {
        0
    };
    let short_open_tag = req
        .ini_get(b"short_open_tag")
        .is_none_or(Request::ini_parse_bool);
    let tokens = if flags & TOKEN_PARSE == 0 {
        tokenize(req, code.as_bytes(), short_open_tag)
    } else {
        tokenize_parse(req, code.as_bytes(), short_open_tag)?
    };
    let mut texts = Texts::for_flags(flags);
    let mut rows = PhpArray::new(req)?;
    for token in tokens {
        let text = code.as_bytes().get(token.start..token.end).unwrap_or(&[]);
        let text = texts.make_str(req, text)?;
        match token.name.and_then(names::id_of) {
            Some(id) => {
                let mut row = PhpArray::new(req)?;
                row.append(req, Value::int(id))?;
                row.append(req, text)?;
                row.append(req, Value::int(i64::from(token.line)))?;
                rows.append(req, Value::array(row))?;
            }
            None => rows.append(req, text)?,
        }
    }
    Ok(Value::array(rows))
}

/// php's `make_str` (`Z/ext/tokenizer/tokenizer.c:266`) and the
/// `interned_strings` table `tokenize()` keys on a token's text: a text
/// of one byte is `ZSTR_CHAR`, the process's own; a text seen before in
/// the same call is `zend_string_copy` of the first string, one more
/// owner and no allocation; a text not yet seen is `zend_string_init`,
/// remembered for the call. `tokenize_parse()` hands `add_token` no
/// table, so under `TOKEN_PARSE` every text past one byte is its own
/// string. The table is the call's, on the C stack in php and freed
/// before the call returns, so it is not a PHP value, not on the request
/// heap and not in `memory_get_usage()`.
enum Texts<'a> {
    /// `tokenize()`'s table: the first string of each text, by its bytes.
    Shared(HashMap<&'a [u8], PhpString>),
    /// `tokenize_parse()`'s `NULL`: no table.
    Fresh,
}

impl<'a> Texts<'a> {
    /// `tokenize_common`'s choice: the table without `TOKEN_PARSE`, none
    /// with it.
    fn for_flags(flags: i64) -> Self {
        if flags & TOKEN_PARSE == 0 {
            Texts::Shared(HashMap::new())
        } else {
            Texts::Fresh
        }
    }

    /// `make_str`: the string a row or a property takes for `text`, one
    /// owner of it handed over, so a repeated text is [`Request::addref`]
    /// over the first call's string.
    fn make_str(&mut self, req: &mut Request, text: &'a [u8]) -> Result<Value, Thrown> {
        let Texts::Shared(table) = self else {
            return Ok(Value::string(PhpString::new(req, text)?));
        };
        if text.len() == 1 {
            return Ok(Value::string(PhpString::new(req, text)?));
        }
        if let Some(first) = table.get(text) {
            let copy = Value::string(*first);
            req.addref(&copy);
            return Ok(copy);
        }
        let first = PhpString::new(req, text)?;
        table.insert(text, first);
        Ok(Value::string(first))
    }
}

/// php's `tokenize()` (`Z/ext/tokenizer/tokenizer.c:321`): the scanner
/// alone, with no parser above it (`PARSER_MODE()` false), so what it
/// throws is cleared (`zend_clear_exception` in `tokenize_common`) and
/// nothing is deprecated, while the one warning its escape reader
/// raises with no parser above it, the octal overflow, is raised at the
/// executed site as `zend_error` places it with no file being compiled.
///
/// ```php
/// var_dump(count(token_get_all('<?php "\777" "\u{xyz}"')));
/// ```
/// ```expect
/// Warning: Octal escape sequence overflow \777 is greater than \377 in %s on line 2
/// int(4)
/// ```
fn tokenize(req: &mut Request, code: &[u8], short_open_tag: bool) -> Vec<tokens::PhpToken> {
    let lexed = syntax::lex(FileId(0), code, &LexOptions { short_open_tag });
    for raised in &lexed.diagnostics {
        if raised.level == syntax::Level::CompileWarning {
            raise(req, Level::CompileWarning, &raised.message);
        }
    }
    tokens::tokens(code, &lexed.tokens)
}

/// php's `tokenize_parse()` (`Z/ext/tokenizer/tokenizer.c:455`): the
/// parser over the scanner, `CG(in_compilation)` set and the empty
/// string as the compiled file name, so what the scanner and the parser
/// raise stands `in  on line N`, the line the scanner had reached. A
/// warning or a deprecation is raised and the parse goes on; a parse
/// error is php's `ParseError` at the compiled site, thrown, and the
/// tokens are not returned. The `(unset)` cast's fatal is the
/// compiler's, which `tokenize_parse` never runs, so it is not raised.
/// What the parser took as a name is put onto the tokens as
/// [`feedback`] reads it; what stops at `__halt_compiler();` stops as
/// the plain tokenize stops, php's `ON_STOP` and its three-token count
/// agreeing there.
///
/// A keyword after `::`, as a method's name, a class constant's, an
/// enum case's, a named argument's or a namespace's, and `readonly` as
/// a function's, is `T_STRING`; `static` as a class reference, `array`
/// and `callable` in a type, and a keyword the grammar reads as itself
/// stay what the scanner said. The two fences pin the member after `::`
/// and the members of a class; `the_parser_s_names_are_php_s` in
/// `tests/capture.rs` holds the rest of the sites to what the pinned
/// php printed.
///
/// ```php
/// var_dump(token_get_all('<?php $x->class;A::list;A::class;', TOKEN_PARSE));
/// ```
/// ```expect
/// array(13) {
///   [0]=>
///   array(3) {
///     [0]=>
///     int(394)
///     [1]=>
///     string(6) "<?php "
///     [2]=>
///     int(1)
///   }
///   [1]=>
///   array(3) {
///     [0]=>
///     int(266)
///     [1]=>
///     string(2) "$x"
///     [2]=>
///     int(1)
///   }
///   [2]=>
///   array(3) {
///     [0]=>
///     int(389)
///     [1]=>
///     string(2) "->"
///     [2]=>
///     int(1)
///   }
///   [3]=>
///   array(3) {
///     [0]=>
///     int(262)
///     [1]=>
///     string(5) "class"
///     [2]=>
///     int(1)
///   }
///   [4]=>
///   string(1) ";"
///   [5]=>
///   array(3) {
///     [0]=>
///     int(262)
///     [1]=>
///     string(1) "A"
///     [2]=>
///     int(1)
///   }
///   [6]=>
///   array(3) {
///     [0]=>
///     int(402)
///     [1]=>
///     string(2) "::"
///     [2]=>
///     int(1)
///   }
///   [7]=>
///   array(3) {
///     [0]=>
///     int(262)
///     [1]=>
///     string(4) "list"
///     [2]=>
///     int(1)
///   }
///   [8]=>
///   string(1) ";"
///   [9]=>
///   array(3) {
///     [0]=>
///     int(262)
///     [1]=>
///     string(1) "A"
///     [2]=>
///     int(1)
///   }
///   [10]=>
///   array(3) {
///     [0]=>
///     int(402)
///     [1]=>
///     string(2) "::"
///     [2]=>
///     int(1)
///   }
///   [11]=>
///   array(3) {
///     [0]=>
///     int(262)
///     [1]=>
///     string(5) "class"
///     [2]=>
///     int(1)
///   }
///   [12]=>
///   string(1) ";"
/// }
/// ```
///
/// ```php
/// var_dump(token_get_all('<?php class C{const array=1;function callable(callable $c):static{}}', TOKEN_PARSE));
/// ```
/// ```expect
/// array(24) {
///   [0]=>
///   array(3) {
///     [0]=>
///     int(394)
///     [1]=>
///     string(6) "<?php "
///     [2]=>
///     int(1)
///   }
///   [1]=>
///   array(3) {
///     [0]=>
///     int(336)
///     [1]=>
///     string(5) "class"
///     [2]=>
///     int(1)
///   }
///   [2]=>
///   array(3) {
///     [0]=>
///     int(397)
///     [1]=>
///     string(1) " "
///     [2]=>
///     int(1)
///   }
///   [3]=>
///   array(3) {
///     [0]=>
///     int(262)
///     [1]=>
///     string(1) "C"
///     [2]=>
///     int(1)
///   }
///   [4]=>
///   string(1) "{"
///   [5]=>
///   array(3) {
///     [0]=>
///     int(312)
///     [1]=>
///     string(5) "const"
///     [2]=>
///     int(1)
///   }
///   [6]=>
///   array(3) {
///     [0]=>
///     int(397)
///     [1]=>
///     string(1) " "
///     [2]=>
///     int(1)
///   }
///   [7]=>
///   array(3) {
///     [0]=>
///     int(262)
///     [1]=>
///     string(5) "array"
///     [2]=>
///     int(1)
///   }
///   [8]=>
///   string(1) "="
///   [9]=>
///   array(3) {
///     [0]=>
///     int(260)
///     [1]=>
///     string(1) "1"
///     [2]=>
///     int(1)
///   }
///   [10]=>
///   string(1) ";"
///   [11]=>
///   array(3) {
///     [0]=>
///     int(310)
///     [1]=>
///     string(8) "function"
///     [2]=>
///     int(1)
///   }
///   [12]=>
///   array(3) {
///     [0]=>
///     int(397)
///     [1]=>
///     string(1) " "
///     [2]=>
///     int(1)
///   }
///   [13]=>
///   array(3) {
///     [0]=>
///     int(262)
///     [1]=>
///     string(8) "callable"
///     [2]=>
///     int(1)
///   }
///   [14]=>
///   string(1) "("
///   [15]=>
///   array(3) {
///     [0]=>
///     int(345)
///     [1]=>
///     string(8) "callable"
///     [2]=>
///     int(1)
///   }
///   [16]=>
///   array(3) {
///     [0]=>
///     int(397)
///     [1]=>
///     string(1) " "
///     [2]=>
///     int(1)
///   }
///   [17]=>
///   array(3) {
///     [0]=>
///     int(266)
///     [1]=>
///     string(2) "$c"
///     [2]=>
///     int(1)
///   }
///   [18]=>
///   string(1) ")"
///   [19]=>
///   string(1) ":"
///   [20]=>
///   array(3) {
///     [0]=>
///     int(321)
///     [1]=>
///     string(6) "static"
///     [2]=>
///     int(1)
///   }
///   [21]=>
///   string(1) "{"
///   [22]=>
///   string(1) "}"
///   [23]=>
///   string(1) "}"
/// }
/// ```
///
/// ```php
/// var_dump(count(token_get_all('<?php (integer)$x; "\777";', TOKEN_PARSE)));
/// token_get_all('<?php if (', TOKEN_PARSE);
/// echo "never\n";
/// ```
/// ```expect exit=255
/// Deprecated: Non-canonical cast (integer) is deprecated, use the (int) cast instead in  on line 1
///
/// Warning: Octal escape sequence overflow \777 is greater than \377 in  on line 1
/// int(7)
///
/// Parse error: Unclosed '(' in  on line 1
/// ```
fn tokenize_parse(
    req: &mut Request,
    code: &[u8],
    short_open_tag: bool,
) -> Result<Vec<tokens::PhpToken>, Thrown> {
    let options = syntax::Options {
        file: FileId(0),
        lex: LexOptions { short_open_tag },
    };
    let parsed = syntax::parse(code, &options);
    for raised in &parsed.diagnostics {
        match raised.level {
            syntax::Level::Parse => {
                return Err(throw_at(
                    req,
                    "ParseError",
                    &raised.message,
                    b"",
                    raised.line,
                ));
            }
            syntax::Level::CompileWarning => {
                raise_at(
                    req,
                    Level::CompileWarning,
                    &raised.message,
                    b"",
                    raised.line,
                );
            }
            syntax::Level::Deprecated => {
                raise_at(req, Level::Deprecated, &raised.message, b"", raised.line);
            }
            syntax::Level::CompileError => {}
        }
    }
    let mut tokens = tokens::tokens(code, &parsed.tokens);
    let fed = feedback::identifiers(&parsed);
    for token in &mut tokens {
        if fed.binary_search(&token.start).is_ok() {
            token.name = Some("T_STRING");
        }
    }
    Ok(tokens)
}

/// `token_name(int $id): string`, php's `PHP_FUNCTION(token_name)`
/// (`Z/ext/tokenizer/tokenizer.c:535`): `get_token_type_name` over the
/// number, `UNKNOWN` for one no token has, `TOKEN_PARSE` and the single
/// characters included, and `T_DOUBLE_COLON` for the number
/// `T_PAAMAYIM_NEKUDOTAYIM` also names.
///
/// ```php
/// var_dump(token_name(T_VARIABLE), token_name(T_PAAMAYIM_NEKUDOTAYIM), token_name(T_BAD_CHARACTER), token_name(T_LNUMBER - 1), token_name(59), token_name(TOKEN_PARSE), token_name(-1), token_name('266'));
/// ```
/// ```expect
/// string(10) "T_VARIABLE"
/// string(14) "T_DOUBLE_COLON"
/// string(15) "T_BAD_CHARACTER"
/// string(7) "UNKNOWN"
/// string(7) "UNKNOWN"
/// string(7) "UNKNOWN"
/// string(7) "UNKNOWN"
/// string(10) "T_VARIABLE"
/// ```
fn token_name(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let id = long_argument(req, 1)?;
    let name = names::name_of(id).unwrap_or("UNKNOWN");
    Ok(Value::string(PhpString::new(req, name.as_bytes())?))
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

/// One `Z_PARAM_STR` of a native, the argument at `position` read off
/// the call frame, `coerce_param` over `string`: `null` is the 8.1
/// deprecation and then the empty string under the weak rule, and the
/// `TypeError` under `strict_types`.
///
/// ```php
/// var_dump(token_get_all(null));
/// ```
/// ```expect
/// Deprecated: token_get_all(): Passing null to parameter #1 ($code) of type string is deprecated in %s on line 2
/// array(0) {
/// }
/// ```
///
/// ```php
/// declare(strict_types=1);
/// try { token_get_all(null); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { token_get_all('<?php 1;', null); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// token_get_all(): Argument #1 ($code) must be of type string, null given
/// token_get_all(): Argument #2 ($flags) must be of type int, null given
/// ```
fn string_argument(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::String, position)?;
    match coerced.as_php_string() {
        Some(s) => Ok(*s),
        None => PhpString::new(req, b""),
    }
}

/// One `Z_PARAM_LONG` of a native, the argument at `position` read off
/// the call frame, `coerce_param` over `int`.
fn long_argument(req: &mut Request, position: u32) -> Result<i64, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::Int, position)?;
    Ok(coerced.as_int().unwrap_or(0))
}

/// One declared parameter, by value, with no default.
fn parameter(name: &[u8], ty: Type) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty: Some(ty),
        by_ref: false,
        prefer_ref: false,
        variadic: false,
        default: None,
        attributes: Vec::new(),
    }
}

/// One native's declaration, as the attributes will derive it from the
/// Rust signature.
fn declared(name: &[u8], parameters: &[Parameter], returns: Type) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.into()),
        parameters: parameters.to_vec(),
        return_type: Some(returns),
        returns_by_ref: false,
        is_generator: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
        frameless: Vec::new(),
    }
}

/// One `REGISTER_LONG_CONSTANT`.
fn constant(name: &[u8], value: i64) -> ConstDecl {
    ConstDecl {
        name: Name(name.into()),
        value: ConstExpr::Long(value),
        attributes: Vec::new(),
    }
}
