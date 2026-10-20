//! The `tokenizer` extension: `php-src/ext/tokenizer` as one crate under
//! `ext/` (ADR-0004). One dependency, the facade; one entry, `extension()`;
//! its state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/tokenizer/tests`, through `xtask phpt --subset`.
//!
//! Two natives, `token_get_all` and `token_name`, over the one lexer
//! (design §4.9) through `php_ext::syntax` and the `T_*` constants of
//! [`names`]. What php's scanner cuts differently from the lexer is put
//! back in the engine's scanner view, which [`tokens`] cuts where
//! `tokenize()` stops; the names the lexer's kinds carry come from
//! `php_names` and are not repeated here. Under `TOKEN_PARSE` the one
//! parser runs over the same stream and what it took as a name is read
//! off the tree in [`feedback`]. The natives, `PhpToken` and the
//! constants are declared by the attributes (design §7.1). `PhpToken` is
//! the one native class: `tokenize` over the same two paths,
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
use std::ops::ControlFlow;

use php_ext::abi::{Extension, ExtensionBuilder, This, Untyped};
use php_ext::attr::{php_class, php_function};
use php_ext::call::new_object;
use php_ext::raising::{Level, Thrown, arg_type_error, raise, raise_at, throw_at, throw_error};
use php_ext::request::Request;
use php_ext::syntax::{self, FileId, LexOptions};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use names::constants::{self, TOKEN_PARSE};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {}

/// The extension value (design §7.1): the two natives, `PhpToken`, the
/// `T_*` constants and `TOKEN_PARSE`, and the state declared on it.
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
    ExtensionBuilder::default()
        .functions(vec![token_get_all::declared(), token_name::declared()])
        .class::<PhpToken>()
        .constants(constants::declared())
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

/// Core's `Stringable`, named for `implements`.
#[php_class(extern)]
pub(crate) struct Stringable;

/// `PhpToken` as `register_class_PhpToken` declares it from
/// `tokenizer.stub.php`: a class implementing `Stringable`, the four
/// public typed properties `id`, `text`, `line` and `pos` with no
/// default, in that order, since the methods read them by slot as
/// `OBJ_PROP_NUM` reads them, and the six methods. `tokenize` is the one
/// called on the class; `__construct` is final.
#[php_class(implements(Stringable))]
pub(crate) struct PhpToken;

#[php_class]
impl PhpToken {
    #[php(property)]
    const id: i64;
    #[php(property)]
    const text: PhpString;
    #[php(property)]
    const line: i64;
    #[php(property)]
    const pos: i64;

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
    fn tokenize(
        req: &mut Request,
        code: PhpString,
        #[php(default = 0)] flags: i64,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let Some(token_class) = req.called_scope() else {
            return Err(throw_error(
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
            let object = match new_object(req, token_class) {
                Ok(object) => object,
                Err(thrown) => {
                    rows.give_up(req);
                    return Err(thrown);
                }
            };
            let pos = i64::try_from(token.start).unwrap_or(i64::MAX);
            let line = i64::from(token.line);
            let filled = match texts.make_str(req, text) {
                Ok(text) => fill_token(req, *object, id, text, line, pos),
                Err(thrown) => Err(thrown),
            };
            if let Err(thrown) = filled {
                object.give_up(req);
                rows.give_up(req);
                return Err(thrown);
            }
            rows.append(req, object.into_value())?;
        }
        Ok(rows)
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
    #[php(final)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        id: i64,
        text: PhpString,
        #[php(default = -1)] line: i64,
        #[php(default = -1)] pos: i64,
    ) -> Result<(), Thrown> {
        // `ZVAL_STR_COPY`: the argument keeps its owner and the property
        // takes one of its own, once every argument has been read.
        let text = Owner::take(req, Value::string(text));
        fill_token(req, this.object(), id, text, line, pos)
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
    fn is(req: &mut Request, this: This<Self>, kind: Untyped<Value>) -> Result<bool, Thrown> {
        let this = this.object();
        let Untyped(kind) = kind;
        let answer = match kind.as_ref() {
            ValueRef::Int(n) => token_id(req, this)? == n,
            ValueRef::Str(s) => token_text(req, this)?.as_bytes() == s.as_bytes(),
            ValueRef::Array(kinds) => {
                let mut id = None;
                let mut text = None;
                // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(kind), entry)`
                // (`Z/ext/tokenizer/tokenizer.c:166`), no owner of the
                // table taken: the argument holds it while a property read
                // or the `TypeError` reaches the program.
                walk::foreach(req, kinds, Hold::None, |req, row| -> walk::Step {
                    let entry = row.value().deref();
                    match entry.as_ref() {
                        ValueRef::Int(n) => {
                            let id = match id {
                                Some(id) => id,
                                None => *id.insert(token_id(req, this)?),
                            };
                            if id == n {
                                return Ok(ControlFlow::Break(()));
                            }
                        }
                        ValueRef::Str(s) => {
                            let text = match text {
                                Some(text) => text,
                                None => *text.insert(token_text(req, this)?),
                            };
                            if text.as_bytes() == s.as_bytes() {
                                return Ok(ControlFlow::Break(()));
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
                    Ok(ControlFlow::Continue(()))
                })?
                .is_some()
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
        Ok(answer)
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
    fn isIgnorable(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let id = token_id(req, this.object())?;
        let ignorable = ["T_WHITESPACE", "T_COMMENT", "T_DOC_COMMENT", "T_OPEN_TAG"]
            .iter()
            .filter_map(|name| names::id_of(name))
            .any(|ignorable| ignorable == id);
        Ok(ignorable)
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
    fn getTokenName(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        let id = token_id(req, this.object())?;
        if id < 256 {
            let byte = u8::try_from(id.rem_euclid(256)).unwrap_or(0);
            return Ok(Some(PhpString::new(req, &[byte])?));
        }
        match names::name_of(id) {
            Some(name) => Ok(Some(PhpString::new(req, name.as_bytes())?)),
            None => Ok(None),
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
    fn __toString(req: &mut Request, this: This<Self>) -> Result<PhpString, Thrown> {
        token_text(req, this.object())
    }
}

/// `ZVAL_LONG(OBJ_PROP_NUM(obj, 0), ...)` and the three after it: the
/// four properties written by slot, no name in hand, `text` the
/// object's from here. What a token costs is what php's costs: the
/// object and its slots, and the text where [`Texts`] has no owner of
/// it yet, which the usage reads while the tokens are held. The peak
/// reads nothing here: php's compiler has raised it past what a program
/// this small tokenizes to, and this engine's compiler does not charge
/// `mm` (design §5.13).
///
/// ```php
/// $b = memory_get_usage(); $t = PhpToken::tokenize('<?php echo 1; echo 2;'); var_dump(memory_get_usage() - $b);
/// ```
/// ```expect
/// int(1560)
/// ```
fn fill_token(
    req: &mut Request,
    object: PhpObject,
    id: i64,
    text: Owner<Value>,
    line: i64,
    pos: i64,
) -> Result<(), Thrown> {
    if let Err(thrown) = object.set_slot(req, 0, Owner::int(id)) {
        text.give_up(req);
        return Err(thrown);
    }
    object.set_slot(req, 1, text)?;
    object.set_slot(req, 2, Owner::int(line))?;
    let pos = Owner::int(pos);
    object.set_slot(req, 3, pos)
}

/// `php_token_get_id`: `id` off the receiver, and php's `Error` for a
/// typed property nothing has written.
fn token_id(req: &mut Request, this: PhpObject) -> Result<i64, Thrown> {
    match this.prop(req, b"id").and_then(|id| id.deref().as_int()) {
        Some(id) => Ok(id),
        None => Err(throw_error(
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
        .and_then(|text| text.deref().as_php_string())
    {
        Some(text) => Ok(text),
        None => Err(throw_error(
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
///
/// `$code` is a `string` parameter: `null` is the 8.1 deprecation and
/// then the empty string under the weak rule, and the `TypeError` under
/// `strict_types`, as `$flags`' is under `int`.
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
#[php_function]
fn token_get_all(
    req: &mut Request,
    code: PhpString,
    #[php(default = 0)] flags: i64,
) -> Result<Owner<PhpArray>, Thrown> {
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
                row.append(req, Owner::int(id))?;
                row.append(req, text)?;
                row.append(req, Owner::int(i64::from(token.line)))?;
                rows.append(req, row.into_value())?;
            }
            None => rows.append(req, text)?,
        }
    }
    Ok(rows)
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
    /// owner of it handed over, so a repeated text is [`Owner::take`] of
    /// the first call's string.
    fn make_str(&mut self, req: &mut Request, text: &'a [u8]) -> Result<Owner<Value>, Thrown> {
        let Texts::Shared(table) = self else {
            return Ok(PhpString::new(req, text)?.into_value());
        };
        if text.len() == 1 {
            return Ok(PhpString::new(req, text)?.into_value());
        }
        if let Some(first) = table.get(text) {
            return Ok(Owner::take(req, Value::string(*first)));
        }
        let first = PhpString::new(req, text)?;
        table.insert(text, *first);
        Ok(first.into_value())
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
fn tokenize(
    req: &mut Request,
    code: &[u8],
    short_open_tag: bool,
) -> Vec<php_ext::syntax::ScannedToken> {
    let lexed = syntax::lex(
        FileId(0),
        code,
        &LexOptions {
            short_open_tag,
            ..LexOptions::default()
        },
    );
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
) -> Result<Vec<php_ext::syntax::ScannedToken>, Thrown> {
    let options = syntax::Options {
        file: FileId(0),
        lex: LexOptions {
            short_open_tag,
            ..LexOptions::default()
        },
    };
    let arena = syntax::Arena::new();
    let parsed = syntax::parse(&arena, code, &options);
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
    let mut tokens = tokens::tokens(code, parsed.tokens);
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
#[php_function]
fn token_name(req: &mut Request, id: i64) -> Result<Owner<PhpString>, Thrown> {
    let name = names::name_of(id).unwrap_or("UNKNOWN");
    PhpString::new(req, name.as_bytes())
}
