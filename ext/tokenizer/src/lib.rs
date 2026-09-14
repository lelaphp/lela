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
//! off the tree in [`feedback`]. `PhpToken` is not declared: a native
//! class waits on the engine serving one.
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

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::coerce::coerce_param;
use php_ext::decl::{ConstDecl, ConstExpr, FunctionDecl, Name, Parameter, Type};
use php_ext::raising::{
    Level, Thrown, arg_count_error, arg_null_deprecated, raise, raise_at, throw_at,
};
use php_ext::request::Request;
use php_ext::syntax::{self, FileId, LexOptions};
use php_ext::values::{PhpArray, PhpString, Value, ValueRef};

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
    let mut constants = vec![constant(b"TOKEN_PARSE", TOKEN_PARSE)];
    for (name, id) in names::TOKENS {
        constants.push(constant(name.as_bytes(), id));
        if name == "T_DOUBLE_COLON" {
            constants.push(constant(b"T_PAAMAYIM_NEKUDOTAYIM", id));
        }
    }
    ExtensionBuilder::default()
        .functions(vec![
            (
                declared(
                    b"token_get_all",
                    &[
                        parameter(b"code", Type::String),
                        parameter(b"flags", Type::Int),
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
        .constants(constants)
        .request_state::<State>()
        .build("tokenizer")
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
/// `ZEND_PARSE_PARAMETERS_START(1, 2)`: `Z_PARAM_STR` over the code and
/// an optional `Z_PARAM_LONG` over the flags, so `1` is the code `"1"`,
/// and a call outside that arity is php's `ArgumentCountError`, which
/// `the_arity_is_php_s` in `tests/capture.rs` holds without the oracle.
/// `flags & TOKEN_PARSE` chooses between [`tokenize`] and
/// [`tokenize_parse`], as `tokenize_common` chooses.
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
    let mut rows = PhpArray::new();
    for token in tokens {
        let text = code.as_bytes().get(token.start..token.end).unwrap_or(&[]);
        let text = Value::string(PhpString::new(req, text)?);
        match token.name.and_then(names::id_of) {
            Some(id) => {
                let mut row = PhpArray::new();
                row.append(Value::int(id));
                row.append(text);
                row.append(Value::int(i64::from(token.line)));
                rows.append(Value::array(row));
            }
            None => rows.append(text),
        }
    }
    Ok(Value::array(rows))
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
/// the call frame: `null` is the 8.1 deprecation and then the empty
/// string, and what is there is coerced as php coerces a string
/// parameter.
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

/// One declared parameter, by value, with no default.
fn parameter(name: &[u8], ty: Type) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty: Some(ty),
        by_ref: false,
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
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
    }
}

/// One `REGISTER_LONG_CONSTANT`.
fn constant(name: &[u8], value: i64) -> ConstDecl {
    ConstDecl {
        name: Name(name.into()),
        value: ConstExpr::Long(value),
    }
}
