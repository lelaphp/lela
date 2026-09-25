//! The syntax group (design §4.4, §4.9): re-exports of the token stream
//! and the tree, the interface `token_get_all`, `highlight_string` and
//! `__COMPILER_HALT_OFFSET__` consume, so an extension reads the lexer's
//! kinds and php's names for them from the one lexer there is, and
//! `token_get_all` under `TOKEN_PARSE` reads the one parser's shape and
//! diagnostics where php's `tokenize_parse` reads the parser's events.
//! What `ext/tokenizer` decides on its own, the `(set)` fusion, `yield
//! from`, `T_BAD_CHARACTER`, the undedented heredoc and which of the
//! tree's names php's grammar takes as an identifier, stays with it.

pub use php_compile::syntax::{
    Diagnostic, Element, FileId, Level, LexOptions, Lexed, Options, Parsed, SyntaxKind, Token,
    TokenId, Tree, amp_name, cast_name, int_overflows, lex, magic_const_name, operator_name, parse,
};

pub use crate::ini_syntax::{IniEvent, IniScanner, IniValue};
pub use crate::scanner::{ScannedToken, scanned};

use crate::api::call::constant;
use crate::api::coerce::to_string;
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::Value;
use crate::ini_syntax::Host;

/// `zend_parse_ini_string` over `text` in a request, the one INI grammar
/// the engine reads its startup entries by: each callback's event in
/// order, or bison's message and the line the scanner stood at. A
/// constant is the request's, a `${…}` reads the request's ini value of
/// the name and then its environment, and a double is written at the
/// request's `precision`. What an extension builds from the events,
/// `php_simple_ini_parser_cb`'s array or `get_browser`'s sections, is
/// its own.
///
/// ```php
/// var_dump(parse_ini_string("tz = UTC;\np = 5 ; x\nq = \"a;b\" ; c\nr = off\ns = E_ALL & ~E_NOTICE"));
/// ```
/// ```expect
/// array(5) {
///   ["tz"]=>
///   string(3) "UTC"
///   ["p"]=>
///   string(1) "5"
///   ["q"]=>
///   string(3) "a;b"
///   ["r"]=>
///   string(0) ""
///   ["s"]=>
///   string(5) "30711"
/// }
/// ```
pub fn parse_ini(
    req: &mut Request,
    text: &[u8],
    scanner: IniScanner,
) -> Result<Result<Vec<IniEvent>, (String, u32)>, Thrown> {
    let mut host = InRequest {
        req,
        events: Vec::new(),
    };
    Ok(crate::ini_syntax::parse(text, scanner, &mut host)?.map(|()| host.events))
}

/// The grammar's host inside a request: the callback collects.
struct InRequest<'r> {
    req: &'r mut Request,
    events: Vec<IniEvent>,
}

impl Host for InRequest<'_> {
    type Error = Thrown;

    fn constant(&mut self, name: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
        match constant(self.req, name)? {
            Some(value) => Ok(Some(to_string(self.req, &value)?.as_bytes().to_vec())),
            None => Ok(None),
        }
    }

    fn directive(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        self.req.ini_get(name).map(<[u8]>::to_vec)
    }

    fn env(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        self.req.env(name).map(<[u8]>::to_vec)
    }

    fn double(&mut self, d: f64) -> Result<Vec<u8>, Thrown> {
        Ok(to_string(self.req, &Value::float(d))?.as_bytes().to_vec())
    }

    fn event(&mut self, event: IniEvent) {
        self.events.push(event);
    }
}
