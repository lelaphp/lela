//! `pcre_get_compiled_regex_cache_ex` (`Z/ext/pcre/php_pcre.c:576`):
//! php's pattern syntax read off the string, the delimiters and the
//! modifiers, compiled once, and the compiled pattern kept in the
//! engine's cache under the string, php's `pcre_cache` of 4,096 entries
//! (design §7.2, ADR-0005). What a request reaches is an `Arc` of
//! the immutable [`Pattern`]; a pattern php refuses is cached as the
//! warning it raises, so a second use raises it again as php's uncached
//! failure does. While the request's `LC_CTYPE` is not `C` the string is
//! keyed behind the locale's name and compiled under the character tables
//! `pcre2_maketables` builds in that locale, cached by the name as php's
//! `char_tables` (design §4.4, revision 56).

use std::sync::Arc;

use php_ext::abi::{Cache, Start};
use php_ext::coerce::is_space;
use php_ext::raising::{Level, Thrown, docref, raise_while_pending};
use php_ext::request::Request;

use crate::constants::PREG_INTERNAL_ERROR;
use crate::ffi::{self, Code, Tables};
use crate::set_error;

/// `PCRE_CACHE_SIZE`.
const CACHE_SIZE: usize = 4096;

/// The locales whose tables are kept at once; php's `char_tables` has no
/// bound, and a pattern keeps the tables it compiled with.
const TABLES_SIZE: usize = 64;

/// One locale's tables, or `None` where the library could not build them.
type Built = Option<Arc<Tables>>;

/// One compiled pattern, a `pcre_cache_entry` less its refcount, which
/// the `Arc` is.
pub(crate) struct Pattern {
    pub(crate) code: Code,
    /// `compile_options & PCRE2_UTF`, the `u` modifier, which is what
    /// php advances an empty match by a character rather than a byte for.
    pub(crate) utf: bool,
    /// `capture_count + 1`, the groups a match can fill, the whole
    /// match being group zero.
    pub(crate) groups: usize,
    /// The name of each group that has one, by number, php's
    /// `subpats_table`; empty for a pattern without names.
    pub(crate) names: Box<[Option<Box<[u8]>>]>,
}

/// What the cache holds for one string: the pattern, or the warning
/// php raised for it.
type Compiled = Result<Arc<Pattern>, String>;

/// The extension's engine state (ADR-0005): the cache, and the three
/// ini entries' startup values, the master column of `php --ri pcre`.
pub(crate) struct Patterns {
    cache: Cache<Box<[u8]>, Compiled>,
    tables: Cache<Box<[u8]>, Built>,
    pub(crate) master: [Vec<u8>; 3],
}

/// The engine state as `Engine::new` builds it.
pub(super) fn engine_state(start: &Start) -> Patterns {
    let master = crate::INI.map(|(name, default)| {
        start
            .ini()
            .entries
            .iter()
            .rev()
            .find(|(n, _)| n.as_slice() == name.as_bytes())
            .map_or_else(
                || default.as_bytes().to_vec(),
                |(_, v)| v.clone().unwrap_or_default(),
            )
    });
    Patterns {
        cache: Cache::new(CACHE_SIZE),
        tables: Cache::new(TABLES_SIZE),
        master,
    }
}

/// `pcre_get_compiled_regex_cache(regex)`: the compiled pattern, or
/// `None` with php's warning raised and `PREG_INTERNAL_ERROR` set. The
/// same string compiles apart under each `LC_CTYPE` the request moves to,
/// and back under `C` as it first did.
///
/// ```php
/// var_dump(preg_match('/\w/', "\xe4"));
/// var_dump(setlocale(LC_CTYPE, "de_DE", "de-DE"), preg_match('/\w/', "\xe4"), preg_match('/^\w\W$/i', "\xc4!"));
/// var_dump(setlocale(LC_CTYPE, "C"), preg_match('/\w/', "\xe4"));
/// var_dump(setlocale(LC_ALL, "de_DE.ISO8859-1"), preg_match('/[[:alpha:]]/', "\xe4"), setlocale(LC_CTYPE, "C.UTF-8"), preg_match('/\w/', "\xe4"));
/// ```
/// ```expect locale=de_DE locale=de_DE.ISO8859-1
/// int(0)
/// string(5) "de_DE"
/// int(1)
/// int(1)
/// string(1) "C"
/// int(0)
/// string(15) "de_DE.ISO8859-1"
/// int(1)
/// string(7) "C.UTF-8"
/// int(1)
/// ```
pub(super) fn compiled(req: &mut Request, regex: &[u8]) -> Option<Arc<Pattern>> {
    compiled_under(req, regex, None)
}

/// [`compiled`] where a throwable may be pending, as php compiles on
/// past one in a loop that does not stop for it: what it warns is
/// raised while the throwable carries, which no user error handler is
/// handed.
pub(super) fn compiled_under(
    req: &mut Request,
    regex: &[u8],
    pending: Option<&Thrown>,
) -> Option<Arc<Pattern>> {
    let warn = |req: &mut Request, message: &str| match pending {
        None => docref(req, Level::Warning, message),
        Some(pending) => {
            let text = format!("{}(): {message}", req.active_function_name());
            raise_while_pending(req, pending, Level::Warning, &text);
        }
    };
    let jit = crate::limits(req).jit;
    let mut jit_failure = None;
    let ctype = req.ctype_locale().map(<[u8]>::to_vec);
    let tables = |patterns: Option<&Patterns>| {
        let name = ctype.as_deref()?;
        let make = || Tables::make().map(Arc::new);
        Some(patterns.map_or_else(make, |p| {
            (*p.tables.get_or_build(name.into(), |_| make())).clone()
        }))
    };
    let built = match req.engine::<Patterns>() {
        Some(patterns) => {
            let key = [ctype.as_deref().unwrap_or_default(), regex].concat();
            patterns.cache.get_or_build(key.into(), |_| {
                compile(regex, tables(Some(patterns)), jit, &mut jit_failure)
            })
        }
        None => Arc::new(compile(regex, tables(None), jit, &mut jit_failure)),
    };
    if let Some(message) = jit_failure {
        warn(req, &message);
        set_error(req, PREG_INTERNAL_ERROR);
    }
    match &*built {
        Ok(pattern) => Some(Arc::clone(pattern)),
        Err(message) => {
            warn(req, message);
            set_error(req, PREG_INTERNAL_ERROR);
            None
        }
    }
}

/// Everything `pcre_get_compiled_regex_cache_ex` does after the cache
/// missed, as a value: the pattern, or the warning. `tables` is `None`
/// in the C locale and `Some` of what `pcre2_maketables` built otherwise.
fn compile(
    regex: &[u8],
    tables: Option<Built>,
    jit: bool,
    jit_failure: &mut Option<String>,
) -> Compiled {
    let (body, modifiers) = split(regex)?;
    let mut options = 0;
    let mut extra = 0;
    let mut utf = false;
    for &modifier in modifiers {
        match modifier {
            b'i' => options |= ffi::PCRE2_CASELESS,
            b'm' => options |= ffi::PCRE2_MULTILINE,
            b'n' => options |= ffi::PCRE2_NO_AUTO_CAPTURE,
            b's' => options |= ffi::PCRE2_DOTALL,
            b'x' => options |= ffi::PCRE2_EXTENDED,
            b'A' => options |= ffi::PCRE2_ANCHORED,
            b'D' => options |= ffi::PCRE2_DOLLAR_ENDONLY,
            b'r' => extra |= ffi::PCRE2_EXTRA_CASELESS_RESTRICT,
            b'S' | b'X' | b' ' | b'\n' | b'\r' => {}
            b'U' => options |= ffi::PCRE2_UNGREEDY,
            b'u' => {
                options |= ffi::PCRE2_UTF | ffi::PCRE2_UCP;
                utf = true;
            }
            b'J' => options |= ffi::PCRE2_DUPNAMES,
            0 => return Err("NUL byte is not a valid modifier".to_owned()),
            other => return Err(format!("Unknown modifier '{}'", char::from(other))),
        }
    }
    let tables = match tables {
        Some(None) => return Err("Failed to generate locale character tables".to_owned()),
        Some(built) => built,
        None => None,
    };
    let mut code = Code::compile(body, options, extra, tables).map_err(|error| {
        format!(
            "Compilation failed: {} at offset {}",
            ffi::error_message(error.number),
            error.offset
        )
    })?;
    if jit && let Err(rc) = code.jit() {
        *jit_failure = Some(format!(
            "JIT compilation failed: {}",
            ffi::error_message(rc)
        ));
    }
    let groups = code.capture_count() as usize + 1;
    let table = code.names();
    let names = if table.is_empty() {
        Box::default()
    } else {
        let mut by_group = vec![None; groups];
        for (group, name) in table {
            if let Some(slot) = by_group.get_mut(group) {
                *slot = Some(name.into_boxed_slice());
            }
        }
        by_group.into_boxed_slice()
    };
    Ok(Arc::new(Pattern {
        code,
        utf,
        groups,
        names,
    }))
}

/// The delimiters: leading whitespace skipped, the delimiter read and
/// refused where it is alphanumeric, a backslash or NUL, a bracket
/// paired with its closer and nested, and the end found past escaped
/// delimiters. The pattern between them and the modifiers after. The
/// whitespace is C's `isspace` under the request's `LC_CTYPE`, as
/// `pcre_get_compiled_regex_cache_ex` reads it, which counts 0xA0 under
/// macOS's `C.UTF-8`, php's default.
///
/// ```php
/// var_dump(preg_match("\xa0/a/", "a"), preg_match(" \v/b/", "b"));
/// ```
/// ```expect
/// int(1)
/// int(1)
/// ```
fn split(regex: &[u8]) -> Result<(&[u8], &[u8]), String> {
    let mut at = regex
        .iter()
        .position(|b| !is_space(*b))
        .ok_or_else(|| "Empty regular expression".to_owned())?;
    let start = regex[at];
    if start.is_ascii_alphanumeric() || start == b'\\' || start == 0 {
        return Err("Delimiter must not be alphanumeric, backslash, or NUL byte".to_owned());
    }
    at += 1;
    let end = match start {
        b'(' => b')',
        b'[' => b']',
        b'{' => b'}',
        b'<' => b'>',
        other => other,
    };
    let body_start = at;
    let mut depth = 1;
    while at < regex.len() {
        let byte = regex[at];
        if byte == b'\\' && at + 1 < regex.len() {
            at += 1;
        } else if byte == end
            && (start == end || {
                depth -= 1;
                depth <= 0
            })
        {
            break;
        } else if byte == start && start != end {
            depth += 1;
        }
        at += 1;
    }
    if at >= regex.len() {
        let which = if start == end { "" } else { "matching " };
        return Err(format!(
            "No ending {which}delimiter '{}' found",
            char::from(end)
        ));
    }
    Ok((&regex[body_start..at], &regex[at + 1..]))
}
