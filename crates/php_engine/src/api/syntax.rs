//! The syntax group (design §4.4, §4.9): re-exports of the token stream
//! and the tree, the interface `token_get_all`, `highlight_string` and
//! `__COMPILER_HALT_OFFSET__` consume, so an extension reads the lexer's
//! kinds and php's names for them from the one lexer there is, and
//! `token_get_all` under `TOKEN_PARSE` reads the one parser's shape and
//! diagnostics where php's `tokenize_parse` reads the parser's events.
//! What `ext/tokenizer` decides on its own, the `(set)` fusion, `yield
//! from`, `T_BAD_CHARACTER`, the undedented heredoc and which of the
//! tree's names php's grammar takes as an identifier, stays with it.
//! `php_getopt`, which `getopt()` reads the program's `argv` with, is the
//! one walk every host reads its own command line with (`main/getopt.c`).

pub use php_compile::syntax::{
    Arena, Diagnostic, Element, FileId, Level, LexOptions, Lexed, Options, Parsed, Position,
    ScannedToken, SyntaxKind, Token, TokenId, Tree, lex, parse, scanned,
};

pub use crate::getopt::{Getopt, Need, Opt, Taken};
pub use crate::ini_syntax::{IniEvent, IniScanner, IniValue};

use crate::api::call::constant;
use crate::api::coerce::with_string;
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Emalloc, PhpString, Value};
use crate::ini_syntax::{Held, Host, Val};
use php_compile::multibyte::{self, Filter, Step};
use php_compile::syntax::Colours;

/// `zend_parse_ini_string` over `text` in a request, the one INI grammar
/// the engine reads its startup entries by: `callback` called with each
/// statement's event as it is reduced, then nothing, or bison's message
/// and the line the scanner stood at. The event's strings are the
/// request's, made as the scanner's `zend_ini_copy_value` and the
/// grammar's actions make them and given up after the callback, so a
/// callback keeps one by taking an owner of it (`Z_TRY_ADDREF`), and the
/// scanner's state stack is allocated as `zend_stack` grows it, which
/// `memory_get_peak_usage()` sees. A
/// constant is the request's, a `${…}` reads what the host's ini text
/// set under the name (`zend_get_configuration_directive`), not the
/// entry's value, and then the environment, and a double is written at
/// the request's `precision`. What an extension builds from the events,
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
///
/// The recipe sets `precision`, and `track_errors`, which php has no
/// entry for, and both are read as it set them; the request's own change
/// of `precision` is not read, and `zend.enable_gc`, an entry at a default
/// no text set, is read empty:
///
/// ```php
/// ini_set('precision', '5');
/// var_dump(parse_ini_string("a = \${precision}\nb = \${track_errors}\nc = \${zend.enable_gc}"));
/// ```
/// ```expect
/// array(3) {
///   ["a"]=>
///   string(2) "14"
///   ["b"]=>
///   string(1) "0"
///   ["c"]=>
///   string(0) ""
/// }
/// ```
pub fn parse_ini(
    req: &mut Request,
    text: &[u8],
    scanner: IniScanner,
    callback: impl FnMut(&mut Request, IniEvent<Value>) -> Result<(), Thrown>,
) -> Result<Result<(), (String, u32)>, Thrown> {
    let mut host = InRequest {
        req,
        strings: Vec::new(),
        stack: None,
        callback,
    };
    let parsed = crate::ini_syntax::parse(text, scanner, &mut host);
    // `shutdown_ini_scanner`, and what bison's destructors free of a
    // parse a syntax error stopped.
    let InRequest {
        req,
        strings,
        stack,
        ..
    } = host;
    for fresh in strings.into_iter().flatten() {
        req.heap_mut().release_string(fresh.finish());
    }
    if let Some(stack) = stack {
        stack.free(req);
    }
    parsed
}

/// The grammar's host inside a request: the strings the parse holds, by
/// [`Held`], the state stack's allocation, and the callback.
struct InRequest<'r, F> {
    req: &'r mut Request,
    strings: Vec<Option<php_heap::FreshStr>>,
    stack: Option<Emalloc>,
    callback: F,
}

impl<F> InRequest<'_, F> {
    /// The string `held` names, taken out.
    fn take(&mut self, held: Held) -> Option<php_heap::FreshStr> {
        self.strings.get_mut(held.0).and_then(Option::take)
    }

    /// A value of the event: the held string handed out as the
    /// request's, kept in `given` to give up after the callback.
    fn value(&mut self, val: Val, given: &mut Vec<PhpString>) -> Result<Value, Thrown> {
        if let Some(fresh) = val.held.and_then(|held| self.take(held)) {
            let s = PhpString::of(fresh.finish());
            given.push(s);
            return Ok(Value::string(s));
        }
        Ok(match val.value {
            // `zend_ini_init_string`'s interned empty string.
            IniValue::Str(bytes) => {
                let s = self.req.string(&bytes)?;
                given.push(s);
                Value::string(s)
            }
            IniValue::Bool(b) => Value::bool(b),
            IniValue::Null => Value::null(),
            IniValue::Long(n) => Value::int(n),
            IniValue::Double(d) => Value::float(d),
        })
    }
}

impl<F> Host for InRequest<'_, F>
where
    F: FnMut(&mut Request, IniEvent<Value>) -> Result<(), Thrown>,
{
    type Error = Thrown;
    fn constant(&mut self, name: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
        match constant(self.req, name)? {
            // `zend_ini_get_constant` releases the copy it converted
            // (`Z/Zend/zend_ini_parser.y:162`).
            Some(value) => with_string(self.req, &value, <[u8]>::to_vec).map(Some),
            None => Ok(None),
        }
    }

    fn directive(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        self.req.shared.directive(name).map(<[u8]>::to_vec)
    }

    fn env(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        self.req.env(name).map(<[u8]>::to_vec)
    }

    fn double(&mut self, d: f64) -> Result<Vec<u8>, Thrown> {
        with_string(self.req, &Value::float(d), <[u8]>::to_vec)
    }

    fn event(&mut self, event: IniEvent<Val>) -> Result<(), Thrown> {
        let mut given = Vec::new();
        let mut failed = None;
        let event = event.map(|val| match self.value(val, &mut given) {
            Ok(value) => value,
            Err(thrown) => {
                failed.get_or_insert(thrown);
                Value::null()
            }
        });
        let called = match failed {
            Some(thrown) => Err(thrown),
            None => (self.callback)(self.req, event),
        };
        for s in given {
            self.req.release(Value::string(s));
        }
        called
    }

    fn hold(&mut self, bytes: &[u8], size: usize) -> Result<Option<Held>, Thrown> {
        let fresh = self.req.heap_mut().string_fresh(size);
        let mut fresh = self.req.fatal_of(fresh)?;
        fresh.bytes_mut()[..bytes.len()].copy_from_slice(bytes);
        fresh.set_len(bytes.len());
        self.strings.push(Some(fresh));
        Ok(Some(Held(self.strings.len() - 1)))
    }

    fn extend(&mut self, held: Held, bytes: &[u8]) -> Result<Option<Held>, Thrown> {
        let Some(fresh) = self.take(held) else {
            return self.hold(bytes, bytes.len());
        };
        let grown = self.req.heap_mut().string_extend(fresh, bytes.len());
        let mut grown = self.req.fatal_of(grown)?;
        grown.bytes_mut().copy_from_slice(bytes);
        if let Some(slot) = self.strings.get_mut(held.0) {
            *slot = Some(grown);
        }
        Ok(Some(held))
    }

    fn free(&mut self, held: Held) {
        if let Some(fresh) = self.take(held) {
            self.req.heap_mut().release_string(fresh.finish());
        }
    }

    fn stack(&mut self, slots: usize) -> Result<(), Thrown> {
        let size = slots * std::mem::size_of::<i32>();
        self.stack = Some(match self.stack.take() {
            Some(stack) => stack.resize(self.req, size)?,
            None => Emalloc::new(self.req, size)?,
        });
        Ok(())
    }
}

/// php's scanner view of `source` as a request's scanner reads a file,
/// `short_open_tag` its `CG(short_tags)`; what the scanner raises is
/// dropped, as `zend_highlight` and `zend_strip` clear it.
fn scanned_file(req: &Request, source: &[u8]) -> Vec<ScannedToken> {
    let short_open_tag = req
        .ini_get(b"short_open_tag")
        .is_none_or(Request::ini_parse_bool);
    let options = LexOptions {
        short_open_tag,
        ..LexOptions::default()
    };
    let lexed = lex(FileId(0), source, &options);
    scanned(source, &lexed.tokens)
}

/// How the scanner is handed a source it highlights or strips: a file,
/// whose encoding `open_file_for_scanning` finds, or a string, which
/// `zend_prepare_string_for_scanning` reads in the internal encoding;
/// both are scanned from `INITIAL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scanning {
    File,
    String,
}

/// `zend_highlight` over `source` under the request's
/// `php_get_highlight_struct`, the five `highlight.*` entries: the HTML
/// `highlight_string()`, `highlight_file()` and `php -s` write. Under
/// `zend.multibyte` the source is read as a compile reads it
/// ([`php_compile::multibyte::open`]), and each token's text passes the
/// scanner's token filter
/// on its way out, as `zend_html_puts` passes it through
/// `LANG_SCNG(output_filter)`.
///
/// ```php
/// ini_set('highlight.keyword', 'green');
/// highlight_string("<p>\n<?php\n// c\n\$a = \"x{\$b}\\t\" . 1 <=> A::B; ?>\nend");
/// ```
/// ```expect
/// <pre><code style="color: #000000">&lt;p&gt;
/// <span style="color: #0000BB">&lt;?php
/// </span><span style="color: #FF8000">// c
/// </span><span style="color: #0000BB">$a </span><span style="color: green">= </span><span style="color: #DD0000">"x</span><span style="color: green">{</span><span style="color: #0000BB">$b</span><span style="color: green">}</span><span style="color: #DD0000">\t" </span><span style="color: green">. </span><span style="color: #0000BB">1 </span><span style="color: green">&lt;=&gt; </span><span style="color: #0000BB">A</span><span style="color: green">::</span><span style="color: #0000BB">B</span><span style="color: green">; </span><span style="color: #0000BB">?&gt;
/// </span>end</code></pre>
/// ```
///
/// # Errors
///
/// The `E_COMPILE_ERROR` that ends the request where the source filter
/// cannot convert the source.
pub fn highlight(req: &mut Request, source: &[u8], scanning: Scanning) -> Result<Vec<u8>, Thrown> {
    let read = |name: &[u8]| req.ini_get(name).unwrap_or_default().to_vec();
    let colours = Colours {
        comment: read(b"highlight.comment"),
        default: read(b"highlight.default"),
        html: read(b"highlight.html"),
        keyword: read(b"highlight.keyword"),
        string: read(b"highlight.string"),
    };
    let opened = self::read(req, source, scanning)?;
    let tokens = scanned_file(req, &opened.text);
    let Some((bound, filter, step)) = opened.tokens else {
        return Ok(php_compile::syntax::highlight(
            &opened.text,
            &tokens,
            &colours,
        ));
    };
    // Each token's text through the token filter, laid end to end, the
    // tokens spanning what their text became.
    let encodings = bound.settings().encodings;
    let mut text = Vec::with_capacity(opened.text.len());
    let tokens: Vec<ScannedToken> = tokens
        .into_iter()
        .map(|token| {
            let written = opened.text.get(token.start..token.end).unwrap_or_default();
            let start = text.len();
            match filter.convert(encodings, step, written) {
                Some(converted) => text.extend_from_slice(&converted),
                None => text.extend_from_slice(written),
            }
            ScannedToken {
                start,
                end: text.len(),
                ..token
            }
        })
        .collect();
    Ok(php_compile::syntax::highlight(&text, &tokens, &colours))
}

/// `zend_strip` over `source`: what `php_strip_whitespace()` and
/// `php -w` write, the comments dropped and each stretch of whitespace one
/// space.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'w'); file_put_contents($f, "<?php\n/* c */ \$a  =  1; // d\n\$b = <<<E\n x\n E;\n\n\techo \$a;\n");
/// var_dump(php_strip_whitespace($f)); unlink($f);
/// ```
/// ```expect
/// string(41) "<?php
///  $a = 1; $b = <<<E
///  x
///  E;
/// echo $a; "
/// ```
///
/// # Errors
///
/// The `E_COMPILE_ERROR` that ends the request where the source filter
/// cannot convert the file.
pub fn strip(req: &mut Request, source: &[u8]) -> Result<Vec<u8>, Thrown> {
    let opened = read(req, source, Scanning::File)?;
    let tokens = scanned_file(req, &opened.text);
    Ok(php_compile::syntax::strip(&opened.text, &tokens))
}

/// What the scanner reads of a source it highlights or strips.
struct Read {
    /// The text the lexer reads.
    text: Vec<u8>,
    /// The functions, the filters and the token filter's step, where the
    /// scanner has one.
    tokens: Option<(crate::multibyte::Bound, Filter, Step)>,
}

/// `open_file_for_scanning` and `zend_prepare_string_for_scanning` over
/// `source` under `zend.multibyte`, as a compile reads it
/// ([`php_compile::multibyte::open`]): the encoding found, the byte order
/// mark taken off and the source filter passed over the text; the bytes
/// as they are without it. A source the filter refuses is php's
/// `zend_error_noreturn(E_COMPILE_ERROR, ...)`, placed where the request
/// is executing.
fn read(req: &mut Request, source: &[u8], scanning: Scanning) -> Result<Read, Thrown> {
    let shared = std::sync::Arc::clone(&req.shared);
    let Some(bound) = crate::multibyte::bound(req, shared.compile_multibyte()) else {
        return Ok(Read {
            text: source.to_vec(),
            tokens: None,
        });
    };
    let settings = bound.settings();
    let from = match scanning {
        Scanning::File => multibyte::Source::File {
            detect_unicode: settings.detect_unicode,
            listed: settings.listed,
        },
        Scanning::String => multibyte::Source::Eval,
    };
    match multibyte::open(settings.encodings, source, from) {
        Ok(opened) => {
            let tokens = opened
                .filter
                .and_then(|filter| Some((filter, filter.tokens?)));
            Ok(Read {
                text: opened.text,
                tokens: tokens.map(|(filter, step)| (bound, filter, step)),
            })
        }
        Err(refused) => {
            let file = req
                .current_file()
                .map_or_else(|| b"Unknown".to_vec(), <[u8]>::to_vec);
            let line = req.current_line();
            req.end_in_compile_time(
                crate::api::raising::Level::CompileError,
                &refused.message(),
                &file,
                line,
            );
            Err(Thrown::of(None))
        }
    }
}
