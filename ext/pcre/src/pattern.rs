//! `pcre_get_compiled_regex_cache_ex` (`Z/ext/pcre/php_pcre.c:576`):
//! php's pattern syntax read off the string, the delimiters and the
//! modifiers, compiled once, and the compiled pattern kept in the
//! engine's cache under the string, php's `pcre_cache` of 4,096 entries
//! (design §7.2, ADR-0005). What a request reaches is an `Arc` of
//! the immutable [`Pattern`]; a pattern php refuses is cached as the
//! warning it raises, so a second use raises it again as php's uncached
//! failure does.

use std::sync::Arc;

use php_ext::abi::{Cache, StartupIni};
use php_ext::raising::{Level, docref};
use php_ext::request::Request;

use crate::ffi::{self, Code};
use crate::{INTERNAL_ERROR, set_error};

/// `PCRE_CACHE_SIZE`.
const CACHE_SIZE: usize = 4096;

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
    pub(crate) master: [Vec<u8>; 3],
}

/// The engine state as `Engine::new` builds it.
pub(super) fn engine_state(ini: &StartupIni) -> Patterns {
    let master = crate::INI.map(|(name, default)| {
        ini.entries
            .iter()
            .rev()
            .find(|(n, _)| n.as_slice() == name.as_bytes())
            .map_or_else(|| default.as_bytes().to_vec(), |(_, v)| v.clone())
    });
    Patterns {
        cache: Cache::new(CACHE_SIZE),
        master,
    }
}

/// `pcre_get_compiled_regex_cache(regex)`: the compiled pattern, or
/// `None` with php's warning raised and `PREG_INTERNAL_ERROR` set.
pub(super) fn compiled(req: &mut Request, regex: &[u8]) -> Option<Arc<Pattern>> {
    let jit = crate::ini_bool(req, "pcre.jit");
    let mut jit_failure = None;
    let built = match req.engine::<Patterns>() {
        Some(patterns) => patterns
            .cache
            .get_or_build(regex.into(), |key| compile(key, jit, &mut jit_failure)),
        None => Arc::new(compile(regex, jit, &mut jit_failure)),
    };
    if let Some(message) = jit_failure {
        docref(req, Level::Warning, &message);
        set_error(req, INTERNAL_ERROR);
    }
    match &*built {
        Ok(pattern) => Some(Arc::clone(pattern)),
        Err(message) => {
            docref(req, Level::Warning, message);
            set_error(req, INTERNAL_ERROR);
            None
        }
    }
}

/// Everything `pcre_get_compiled_regex_cache_ex` does after the cache
/// missed, as a value: the pattern, or the warning.
fn compile(regex: &[u8], jit: bool, jit_failure: &mut Option<String>) -> Compiled {
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
    let mut code = Code::compile(body, options, extra).map_err(|error| {
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
/// delimiters. The pattern between them and the modifiers after.
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

/// C's `isspace` in the C locale.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}
