//! The `pcre` extension: `php-src/ext/pcre` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade, and the library php links,
//! libpcre2-8 through `pcre2-sys` with `pcre2_jit_compile` on (design §7.2); one entry,
//! `extension()`; its state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/pcre/tests`, through `xtask phpt --subset`.
//!
//! Declared as `php_pcre.stub.php` at the tag declares it: the nineteen
//! constants `register_php_pcre_symbols` registers, in its order, by
//! `#[php_const]`, the eleven functions by `#[php_function]` from their
//! Rust signatures, and the three ini entries. The bodies are
//! `php_pcre.c`'s. The state is split where php splits it at
//! `RSHUTDOWN` (design §2.3): compiled patterns live in the engine's
//! cache of immutable values, [`pattern::Patterns`], and the match data,
//! `mctx` and `PCRE_G(error_code)` are the request's,
//! [`State`]. The `unsafe` the library needs is `src/ffi/`'s alone.
//! Beyond `extension()` the crate exports [`Regex`], the compiled-pattern
//! face ext/spl and ext/filter read (design §4.10).
#![deny(unsafe_code)]
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

mod ffi;
/// `PCRE2_MAJOR` and `PCRE2_MINOR` of the `pcre2.h` this build compiled
/// against, which `build.rs` reads as the C preprocessor expands them, as
/// php's `@cvalue` reads the macros at its build (#1978).
mod header {
    include!(concat!(env!("OUT_DIR"), "/header.rs"));
}
mod matching;
mod pattern;
mod regex;
mod replace;
mod split;

pub use regex::Regex;

use php_ext::abi::{Extension, ExtensionBuilder, Union};
use php_ext::attr::{php_const, php_function};
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::{Ini, Request};
use php_ext::values::{Emalloc, Owner, PhpArray, PhpString};

use constants::{
    PREG_BACKTRACK_LIMIT_ERROR, PREG_BAD_UTF8_ERROR, PREG_BAD_UTF8_OFFSET_ERROR,
    PREG_INTERNAL_ERROR, PREG_JIT_STACKLIMIT_ERROR, PREG_NO_ERROR, PREG_RECURSION_LIMIT_ERROR,
};

/// The three ini entries and their defaults, `PHP_INI_ALL` each.
const INI: [(&str, &str); 3] = [
    ("pcre.backtrack_limit", "1000000"),
    ("pcre.recursion_limit", "100000"),
    ("pcre.jit", "1"),
];

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised. `PCRE_G(error_code)`, and php's `mdata` and `mctx`:
/// the match data of 32 pairs a call borrows while no other call holds
/// it, and `mctx` with its `jit_stack`, each built on first
/// use and taken out while a call runs, so a call made from a callback
/// builds its own, as php's `mdata_used` has it.
#[derive(Default)]
struct State {
    error_code: i64,
    data: Option<ffi::MatchData>,
    context: Option<ffi::Context>,
    limits: Limits,
    /// `PCRE_G(gctx_zmm)`'s room: the `pcre2_general_context` `RINIT`
    /// creates through `php_pcre_emalloc` and `RSHUTDOWN` frees, which
    /// the engine's match data does not allocate through.
    general: Option<Emalloc>,
    /// `PCRE_G(unmatched_empty_pair)` and `PCRE_G(unmatched_null_pair)`
    /// ([`matching::unmatched_pair`]).
    unmatched: Unmatched,
}

/// The two pairs `add_offset_pair` hands every group that did not take
/// part under `PREG_OFFSET_CAPTURE`, one owner each, `["", -1]` and,
/// under `PREG_UNMATCHED_AS_NULL`, `[null, -1]`: made on first use and
/// given up in `RSHUTDOWN`. Once the program has ended, a pair not
/// cached yet is made fresh each time, as php makes one under
/// `EG_FLAGS_IN_SHUTDOWN`, which `php_request_shutdown` sets before the
/// shutdown functions are called.
#[derive(Default)]
struct Unmatched {
    empty: Option<Owner<PhpArray>>,
    null: Option<Owner<PhpArray>>,
    ended: bool,
}

impl Unmatched {
    /// The slot of the pair `PREG_UNMATCHED_AS_NULL` picks.
    fn slot(&mut self, as_null: bool) -> &mut Option<Owner<PhpArray>> {
        if as_null {
            &mut self.null
        } else {
            &mut self.empty
        }
    }

    /// The cached pair of the kind, a view, where one is cached.
    fn cached(&mut self, as_null: bool) -> Option<PhpArray> {
        self.slot(as_null).as_ref().map(|pair| **pair)
    }

    /// `pair` cached as the kind's, where the program has not ended;
    /// handed back otherwise, the caller's to hand on.
    fn cache(&mut self, as_null: bool, pair: Owner<PhpArray>) -> Result<PhpArray, Owner<PhpArray>> {
        if self.ended || self.slot(as_null).is_some() {
            return Err(pair);
        }
        let view = *pair;
        *self.slot(as_null) = Some(pair);
        Ok(view)
    }

    /// No pair is cached from here, and the two cached are handed back
    /// to be given up.
    fn end(&mut self) -> [Option<Owner<PhpArray>>; 2] {
        self.ended = true;
        [self.empty.take(), self.null.take()]
    }
}

/// The program has ended (`EG(flags) |= EG_FLAGS_IN_SHUTDOWN`): the
/// cached pairs stand until `RSHUTDOWN`, and no other is cached.
fn program_ended(req: &mut Request) {
    req.state::<State>().unmatched.ended = true;
}

/// `sizeof(pcre2_general_context)`: the allocator's two callbacks and
/// their data.
const GENERAL_CONTEXT: usize = 24;

/// `PCRE_G(backtrack_limit)`, `PCRE_G(recursion_limit)` and
/// `PCRE_G(jit)`: the three ini entries as their `OnUpdate*` left them,
/// laid from the entries as the request starts ([`rinit`]) and kept by
/// each entry's `on_modify`, so a match reads them and looks no entry up.
/// A limit is the `uint32_t` php hands `pcre2_set_match_limit` and
/// `pcre2_set_depth_limit`. `ini_set()` and `ini_restore()` take effect
/// on the next match, and a value that is no quantity is php's warning:
///
/// ```php
/// $re = '/(?:\D+|<\d+>)*[!?]/'; $s = 'foobar foo';
/// var_dump(ini_set('pcre.backtrack_limit', '2x'), preg_match($re, $s), preg_last_error_msg());
/// ini_restore('pcre.backtrack_limit');
/// var_dump(preg_match($re, $s), preg_last_error_msg());
/// var_dump(ini_set('pcre.jit', '0'), ini_set('pcre.recursion_limit', '2'), preg_match('/(a)+$/', 'aaaaabc'), preg_last_error_msg());
/// ini_restore('pcre.recursion_limit');
/// var_dump(preg_match('/(a)+$/', 'aaaaabc'), preg_last_error_msg(), ini_get('pcre.jit'));
/// ini_restore('pcre.jit');
/// var_dump(ini_get('pcre.jit'));
/// ```
/// ```expect capture
/// Warning: Invalid "pcre.backtrack_limit" setting. Invalid quantity "2x": unknown multiplier "x", interpreting as "2" for backwards compatibility in %SCRATCH%/witness.php on line 3
/// string(7) "1000000"
/// bool(false)
/// string(25) "Backtrack limit exhausted"
/// int(0)
/// string(8) "No error"
/// string(1) "1"
/// string(6) "100000"
/// bool(false)
/// string(25) "Recursion limit exhausted"
/// int(0)
/// string(8) "No error"
/// string(1) "0"
/// string(1) "1"
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Limits {
    backtrack: u32,
    recursion: u32,
    jit: bool,
}

impl Default for Limits {
    /// The entries' defaults ([`INI`]), what a request holds until
    /// [`rinit`] lays the values in force.
    fn default() -> Limits {
        let mut limits = Limits {
            backtrack: 0,
            recursion: 0,
            jit: false,
        };
        for (name, default) in INI {
            limits.update(name.as_bytes(), default.as_bytes(), Ini::parse_bytes);
        }
        limits
    }
}

impl Limits {
    /// The entry `name` holding `value`, as its `OnUpdate*` reads it:
    /// `OnUpdateLong`'s quantity for a limit, read by `quantity`, and
    /// `OnUpdateBool` for `pcre.jit`.
    fn update(&mut self, name: &[u8], value: &[u8], quantity: impl FnOnce(&[u8]) -> i64) {
        let held = match name {
            b"pcre.backtrack_limit" => &mut self.backtrack,
            b"pcre.recursion_limit" => &mut self.recursion,
            b"pcre.jit" => {
                self.jit = Ini::parse_bool(value);
                return;
            }
            _ => return,
        };
        *held = limit(quantity(value));
    }
}

/// `(uint32_t)PCRE_G(backtrack_limit)`: the low 32 bits, as C's cast.
fn limit(value: i64) -> u32 {
    u32::try_from(value.rem_euclid(1 << 32)).unwrap_or(u32::MAX)
}

/// `PHP_RINIT_FUNCTION(pcre)`: the request's `PCRE_G` limits, as the
/// entries in force give them, and `PCRE_G(gctx_zmm)` created on the
/// request's heap, 24 bytes `memory_get_usage()` counts from the first
/// line. The `Err` is the memory limit's fatal.
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    let mut limits = Limits::default();
    for (name, _) in INI {
        if let Some(value) = req.ini().get(name.as_bytes()) {
            limits.update(name.as_bytes(), value, Ini::parse_bytes);
        }
    }
    req.state::<State>().limits = limits;
    let general = Emalloc::new(req, GENERAL_CONTEXT)?;
    req.state::<State>().general = Some(general);
    Ok(())
}

/// `PHP_RSHUTDOWN_FUNCTION(pcre)`: `pcre2_general_context_free` of
/// `PCRE_G(gctx_zmm)`.
fn rshutdown(req: &mut Request) {
    if let Some(general) = req.state::<State>().general.take() {
        general.free(req);
    }
    // `zval_ptr_dtor(&PCRE_G(unmatched_null_pair))` and its twin.
    let pairs = req.state::<State>().unmatched.end();
    for pair in pairs.into_iter().flatten() {
        pair.give_up(req);
    }
}

/// `OnUpdateBacktrackLimit`, `OnUpdateRecursionLimit` and `OnUpdateJit`
/// (`Z/ext/pcre/php_pcre.c:315-343`): the value taken into the
/// request's [`Limits`], a limit read as `zend_ini_parse_quantity_warn`
/// reads it, with its warning; none is ever refused.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the `on_modify` slot's shape, which a handler that throws needs"
)]
fn on_update<const AT: usize>(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let (name, _) = INI[AT];
    let mut limits = req.state::<State>().limits;
    limits.update(name.as_bytes(), value.unwrap_or_default(), |value| {
        req.ini_mut().parse_quantity_warn(name.as_bytes(), value)
    });
    req.state::<State>().limits = limits;
    Ok(true)
}

/// The request's `PCRE_G` limits ([`Limits`]).
fn limits(req: &mut Request) -> Limits {
    req.state::<State>().limits
}

/// The extension value (design §7.1): the constants, the eleven
/// functions in the stub's order, each declared by `#[php_function]`
/// from its Rust signature, `preg_match` frameless for two arguments and
/// `preg_replace` for three, the ini entries and the two states.
///
/// ```php
/// var_dump(extension_loaded('pcre'), ini_get('pcre.backtrack_limit'), ini_get('pcre.recursion_limit'), ini_get('pcre.jit'));
/// ```
/// ```expect
/// bool(true)
/// string(7) "1000000"
/// string(6) "100000"
/// string(1) "1"
/// ```
///
/// Three of the declarations as Reflection renders them.
///
/// ```php
/// echo new ReflectionFunction('preg_match'), new ReflectionFunction('preg_replace_callback'), new ReflectionFunction('preg_quote');
/// ```
/// ```expect
/// Function [ <internal:pcre> function preg_match ] {
///
///   - Parameters [5] {
///     Parameter #0 [ <required> string $pattern ]
///     Parameter #1 [ <required> string $subject ]
///     Parameter #2 [ <optional> &$matches = null ]
///     Parameter #3 [ <optional> int $flags = 0 ]
///     Parameter #4 [ <optional> int $offset = 0 ]
///   }
///   - Return [ int|false ]
/// }
/// Function [ <internal:pcre> function preg_replace_callback ] {
///
///   - Parameters [6] {
///     Parameter #0 [ <required> array|string $pattern ]
///     Parameter #1 [ <required> callable $callback ]
///     Parameter #2 [ <required> array|string $subject ]
///     Parameter #3 [ <optional> int $limit = -1 ]
///     Parameter #4 [ <optional> &$count = null ]
///     Parameter #5 [ <optional> int $flags = 0 ]
///   }
///   - Return [ array|string|null ]
/// }
/// Function [ <internal:pcre> function preg_quote ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $str ]
///     Parameter #1 [ <optional> ?string $delimiter = null ]
///   }
///   - Return [ string ]
/// }
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .constants(constants::declared())
        .functions(&[
            matching::preg_match::DECLARED,
            matching::preg_match_all::DECLARED,
            replace::preg_replace::DECLARED,
            replace::preg_filter::DECLARED,
            replace::preg_replace_callback::DECLARED,
            replace::preg_replace_callback_array::DECLARED,
            split::preg_split::DECLARED,
            preg_quote::DECLARED,
            split::preg_grep::DECLARED,
            preg_last_error::DECLARED,
            preg_last_error_msg::DECLARED,
        ])
        .ini(
            INI.iter()
                .map(|(name, default)| IniDecl {
                    name: Name(name.as_bytes().into()),
                    default: Some(default.as_bytes().into()),
                    user: true,
                    per_dir: true,
                    system: true,
                })
                .collect(),
        )
        .on_modify(INI[0].0.as_bytes(), on_update::<0>)
        .on_modify(INI[1].0.as_bytes(), on_update::<1>)
        .on_modify(INI[2].0.as_bytes(), on_update::<2>)
        .request_state::<State>()
        .rinit(rinit)
        .rshutdown(rshutdown)
        .program_ended(program_ended)
        .engine_state(|start| Ok(pattern::engine_state(start)))
        .info(pcre_info)
        .build("pcre")
}

/// The constants `register_php_pcre_symbols` registers, in its order.
/// `PCRE_VERSION` is the linked library's, `pcre2_config`'s, so the keg's;
/// its two numbers are `PCRE2_MAJOR` and `PCRE2_MINOR`, the header a build
/// compiled against (see [`header`]), which may be older than the library
/// it loads: the pinned oracle's header is 10.47's though the keg it links
/// is 10.49 (`crosscheck:pcre`), and `pcre2-sys`'s bindings carry 10.45's.
/// The library's Unicode is the keg's too: U+10940, a letter Unicode 17 assigns, is `\p{L}` and
/// `\p{Sidetic}` under `/u`.
///
/// ```php
/// var_dump(PREG_PATTERN_ORDER, PREG_SET_ORDER, PREG_OFFSET_CAPTURE, PREG_UNMATCHED_AS_NULL, PREG_SPLIT_NO_EMPTY, PREG_SPLIT_DELIM_CAPTURE, PREG_SPLIT_OFFSET_CAPTURE, PREG_GREP_INVERT, PREG_NO_ERROR, PREG_INTERNAL_ERROR, PREG_BACKTRACK_LIMIT_ERROR, PREG_RECURSION_LIMIT_ERROR, PREG_BAD_UTF8_ERROR, PREG_BAD_UTF8_OFFSET_ERROR, PREG_JIT_STACKLIMIT_ERROR, PCRE_VERSION_MAJOR, PCRE_JIT_SUPPORT, is_string(PCRE_VERSION));
/// var_dump(PCRE_VERSION, is_int(PCRE_VERSION_MINOR), PCRE_VERSION_MINOR >= 47 && PCRE_VERSION_MINOR <= (int) explode('.', PCRE_VERSION)[1], preg_match('/^\p{L}$/u', "\u{10940}"), preg_match('/^\p{Sidetic}$/u', "\u{10940}"));
/// ```
/// ```expect
/// int(1)
/// int(2)
/// int(256)
/// int(512)
/// int(1)
/// int(2)
/// int(4)
/// int(1)
/// int(0)
/// int(1)
/// int(2)
/// int(3)
/// int(4)
/// int(5)
/// int(6)
/// int(10)
/// bool(true)
/// bool(true)
/// string(16) "10.49 2026-09-28"
/// bool(true)
/// bool(true)
/// int(1)
/// int(1)
/// ```
#[php_const]
pub(crate) mod constants {
    use crate::ffi;

    pub(crate) const PREG_PATTERN_ORDER: i64 = 1;
    pub(crate) const PREG_SET_ORDER: i64 = 2;
    pub(crate) const PREG_OFFSET_CAPTURE: i64 = 1 << 8;
    pub(crate) const PREG_UNMATCHED_AS_NULL: i64 = 1 << 9;
    pub(crate) const PREG_SPLIT_NO_EMPTY: i64 = 1 << 0;
    pub(crate) const PREG_SPLIT_DELIM_CAPTURE: i64 = 1 << 1;
    pub(crate) const PREG_SPLIT_OFFSET_CAPTURE: i64 = 1 << 2;
    pub(crate) const PREG_GREP_INVERT: i64 = 1 << 0;
    pub(crate) const PREG_NO_ERROR: i64 = 0;
    pub(crate) const PREG_INTERNAL_ERROR: i64 = 1;
    pub(crate) const PREG_BACKTRACK_LIMIT_ERROR: i64 = 2;
    pub(crate) const PREG_RECURSION_LIMIT_ERROR: i64 = 3;
    pub(crate) const PREG_BAD_UTF8_ERROR: i64 = 4;
    pub(crate) const PREG_BAD_UTF8_OFFSET_ERROR: i64 = 5;
    pub(crate) const PREG_JIT_STACKLIMIT_ERROR: i64 = 6;

    /// The linked library's version, the keg's `"10.49 2026-09-28"`.
    pub(crate) fn PCRE_VERSION() -> String {
        ffi::version()
    }
    /// `PCRE_VERSION_MAJOR`, `@cvalue PCRE2_MAJOR`: the header this build
    /// compiled against, not the library it loads.
    pub(crate) const PCRE_VERSION_MAJOR: i64 = super::header::PCRE2_MAJOR;
    /// `PCRE_VERSION_MINOR`, `@cvalue PCRE2_MINOR`, of the same header.
    pub(crate) const PCRE_VERSION_MINOR: i64 = super::header::PCRE2_MINOR;
    /// `PCRE_JIT_SUPPORT`, `@cvalue PHP_PCRE_JIT_SUPPORT`: the macro
    /// `php_pcre.c` defines from `HAVE_PCRE_JIT_SUPPORT`, as `configure`
    /// decided it at build: `build.rs` compiles and starts `config0.m4`'s
    /// program against the library and sets the cfg `pcre_jit` where its
    /// `PCRE2_CONFIG_JIT` is 1, not `pcre2_config` of the library loaded
    /// later.
    pub(crate) const PCRE_JIT_SUPPORT: bool = cfg!(pcre_jit);
}

/// What `Z_PARAM_ARRAY_HT_OR_STR` holds, `array|string`: the pattern,
/// replacement and subject of the replacing natives.
pub(crate) type Either = Union<PhpArray, PhpString>;

/// `PCRE_G(error_code) = code`.
fn set_error(req: &mut Request, code: i64) {
    req.state::<State>().error_code = code;
}

/// `PCRE_G(error_code)`.
fn error_code(req: &mut Request) -> i64 {
    req.state::<State>().error_code
}

/// `pcre_handle_exec_error`: the library's code as php's.
fn exec_error(req: &mut Request, rc: i32) {
    let code = match rc {
        ffi::PCRE2_ERROR_MATCHLIMIT => PREG_BACKTRACK_LIMIT_ERROR,
        ffi::PCRE2_ERROR_RECURSIONLIMIT => PREG_RECURSION_LIMIT_ERROR,
        ffi::PCRE2_ERROR_BADUTFOFFSET => PREG_BAD_UTF8_OFFSET_ERROR,
        ffi::PCRE2_ERROR_JIT_STACKLIMIT => PREG_JIT_STACKLIMIT_ERROR,
        rc if (ffi::PCRE2_ERROR_UTF8_ERR21..=ffi::PCRE2_ERROR_UTF8_ERR1).contains(&rc) => {
            PREG_BAD_UTF8_ERROR
        }
        _ => PREG_INTERNAL_ERROR,
    };
    set_error(req, code);
}

/// `php_pcre_get_error_msg`.
fn error_message(code: i64) -> &'static str {
    match code {
        PREG_NO_ERROR => "No error",
        PREG_INTERNAL_ERROR => "Internal error",
        PREG_BAD_UTF8_ERROR => "Malformed UTF-8 characters, possibly incorrectly encoded",
        PREG_BAD_UTF8_OFFSET_ERROR => {
            "The offset did not correspond to the beginning of a valid UTF-8 code point"
        }
        PREG_BACKTRACK_LIMIT_ERROR => "Backtrack limit exhausted",
        PREG_RECURSION_LIMIT_ERROR => "Recursion limit exhausted",
        PREG_JIT_STACKLIMIT_ERROR => "JIT stack limit exhausted",
        _ => "Unknown error",
    }
}

/// `preg_last_error(): int`, `PCRE_G(error_code)`: what the last call
/// of the family left, `PREG_NO_ERROR` while none ran. The backtrack
/// limit is the match limit `pcre2_jit_match` honours as `pcre2_match` does;
/// the recursion limit is the interpreter's depth limit.
///
/// ```php
/// var_dump(preg_last_error(), preg_last_error_msg());
/// var_dump(preg_match('/(?:\D+|<\d+>)*[!?]/', 'foobar foobar foobar'), preg_last_error(), preg_last_error_msg());
/// ini_set('pcre.jit', '0'); ini_set('pcre.recursion_limit', '2');
/// var_dump(preg_match('/(a)+b/', 'aaaab'), preg_last_error(), preg_last_error_msg());
/// ini_set('pcre.recursion_limit', '100000'); ini_set('pcre.backtrack_limit', '10');
/// var_dump(preg_match('/(a+)+$/', str_repeat('a', 20) . 'b'), preg_last_error(), preg_last_error_msg());
/// ```
/// ```expect
/// int(0)
/// string(8) "No error"
/// bool(false)
/// int(2)
/// string(25) "Backtrack limit exhausted"
/// bool(false)
/// int(3)
/// string(25) "Recursion limit exhausted"
/// bool(false)
/// int(2)
/// string(25) "Backtrack limit exhausted"
/// ```
///
/// Bad UTF-8 under `/u`, and an offset inside a character.
///
/// ```php
/// var_dump(preg_match('/./u', "\xff"), preg_last_error(), preg_last_error_msg());
/// var_dump(preg_match('/./u', "\xc3\xa9", $m, 0, 1), preg_last_error(), preg_last_error_msg(), preg_match('/a/', 'a'), preg_last_error());
/// ```
/// ```expect
/// bool(false)
/// int(4)
/// string(56) "Malformed UTF-8 characters, possibly incorrectly encoded"
/// bool(false)
/// int(5)
/// string(74) "The offset did not correspond to the beginning of a valid UTF-8 code point"
/// int(1)
/// int(0)
/// ```
#[php_function]
fn preg_last_error(req: &mut Request) -> i64 {
    error_code(req)
}

/// `preg_last_error_msg(): string`, `php_pcre_get_error_msg` over
/// `PCRE_G(error_code)`, beside `preg_last_error`'s witnesses: a pattern
/// php refuses is an internal error, and the next call that runs clears it.
///
/// ```php
/// var_dump(@preg_match('/(/', 'a'), preg_last_error_msg(), preg_quote('x'), preg_last_error_msg(), preg_split('/,/', 'a'), preg_last_error_msg());
/// ```
/// ```expect
/// bool(false)
/// string(14) "Internal error"
/// string(1) "x"
/// string(14) "Internal error"
/// array(1) {
///   [0]=>
///   string(1) "a"
/// }
/// string(8) "No error"
/// ```
#[php_function]
fn preg_last_error_msg(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let code = error_code(req);
    PhpString::new(req, error_message(code).as_bytes())
}

/// `preg_quote(string $str, ?string $delimiter = null): string`: each
/// byte of `. \ + * ? [ ^ ] $ ( ) { } = ! > < | : - #` and the
/// delimiter's first byte behind a backslash, and NUL as `\000`; the
/// string itself, with an owner of its own, where nothing needed quoting
/// (`RETURN_STR_COPY(str)`).
///
/// ```php
/// var_dump(preg_quote('Hello.World?(1+1=2) #x'), preg_quote('a/b', '/'), preg_quote("a\0b"), preg_quote(''), preg_quote('abc', null), preg_quote('a-b:c', 'bc'));
/// ```
/// ```expect
/// string(29) "Hello\.World\?\(1\+1\=2\) \#x"
/// string(4) "a\/b"
/// string(6) "a\000b"
/// string(0) ""
/// string(3) "abc"
/// string(8) "a\-\b\:c"
/// ```
///
/// So a string that is not interned outlives every call that handed it
/// back: each call's return is given up, and the variable's string is
/// still its own.
///
/// ```php
/// function f($x) { $x = preg_quote($x, '#'); }
/// $p = trim('/up', '/'); for ($i = 0; $i < 3; $i++) f($p);
/// var_dump($p, preg_quote($p), $p);
/// ```
/// ```expect
/// string(2) "up"
/// string(2) "up"
/// string(2) "up"
/// ```
#[php_function]
fn preg_quote(
    req: &mut Request,
    str: PhpString,
    #[php(default = null)] delimiter: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let text = str;
    let delimiter = delimiter.map(|s| s.as_bytes().first().copied().unwrap_or(0));
    let mut quoted = Vec::with_capacity(text.len());
    for &byte in text.as_bytes() {
        match byte {
            b'.' | b'\\' | b'+' | b'*' | b'?' | b'[' | b'^' | b']' | b'$' | b'(' | b')' | b'{'
            | b'}' | b'=' | b'!' | b'>' | b'<' | b'|' | b':' | b'-' | b'#' => {
                quoted.extend_from_slice(&[b'\\', byte]);
            }
            0 => quoted.extend_from_slice(b"\\000"),
            _ => {
                if Some(byte) == delimiter {
                    quoted.push(b'\\');
                }
                quoted.push(byte);
            }
        }
    }
    if quoted.len() == text.len() {
        return Ok(Owner::take(req, text));
    }
    PhpString::new(req, &quoted)
}

/// `PHP_MINFO_FUNCTION(pcre)`: the library's version, its Unicode
/// version and, where the build has `PCRE_JIT_SUPPORT`, the
/// loaded library's `PCRE2_CONFIG_JIT` and its target, then
/// `DISPLAY_INI_ENTRIES`, `pcre.jit` shown as the boolean displayer shows
/// it.
///
/// ```php
/// ob_start(); (new ReflectionExtension('pcre'))->info(); $info = ob_get_clean();
/// echo preg_replace('/(Version|Target) => .*/', '$1 => x', $info);
/// ```
/// ```expect
///
/// pcre
///
/// PCRE (Perl Compatible Regular Expressions) Support => enabled
/// PCRE Library Version => x
/// PCRE Unicode Version => x
/// PCRE JIT Support => enabled
/// PCRE JIT Target => x
///
/// Directive => Local Value => Master Value
/// pcre.backtrack_limit => 1000000 => 1000000
/// pcre.recursion_limit => 100000 => 100000
/// pcre.jit => On => On
/// ```
fn pcre_info(req: &mut Request) {
    let version = ffi::version();
    let unicode = ffi::unicode_version();
    // `#ifdef HAVE_PCRE_JIT_SUPPORT`: the loaded library's answer and
    // target where the build has the JIT, "not compiled in" where not.
    let (jit, target) = if cfg!(pcre_jit) {
        let jit = if ffi::jit_supported() {
            "enabled"
        } else {
            "disabled"
        };
        (jit, ffi::jit_target())
    } else {
        ("not compiled in", None)
    };
    let mut rows: Vec<[&str; 2]> = vec![
        [
            "PCRE (Perl Compatible Regular Expressions) Support",
            "enabled",
        ],
        ["PCRE Library Version", &version],
        ["PCRE Unicode Version", &unicode],
        ["PCRE JIT Support", jit],
    ];
    if let Some(target) = &target {
        rows.push(["PCRE JIT Target", target]);
    }
    let rows: Vec<&[&str]> = rows.iter().map(<[&str; 2]>::as_slice).collect();
    req.extensions_mut().info_table(&rows);
    let master = req
        .engine::<pattern::Patterns>()
        .map(|patterns| patterns.master.clone());
    let cells: Vec<[String; 3]> = INI
        .iter()
        .enumerate()
        .map(|(at, (name, default))| {
            let local = req.ini().get(name.as_bytes()).unwrap_or(default.as_bytes());
            let master = master
                .as_ref()
                .map_or(default.as_bytes(), |m| m[at].as_slice());
            let boolean = *name == "pcre.jit";
            [
                (*name).to_owned(),
                shown(local, boolean),
                shown(master, boolean),
            ]
        })
        .collect();
    let borrowed: Vec<[&str; 3]> = cells
        .iter()
        .map(|[a, b, c]| [a.as_str(), b.as_str(), c.as_str()])
        .collect();
    req.ini_mut().info_ini_table(&borrowed);
}

/// An ini value as its displayer shows it: `On` or `Off` through
/// `zend_ini_boolean_displayer_cb`, and the value as it is otherwise,
/// which `display_ini_entries` shows as `no value` where it is empty.
fn shown(value: &[u8], boolean: bool) -> String {
    if boolean {
        return if Ini::parse_bool(value) { "On" } else { "Off" }.to_owned();
    }
    String::from_utf8_lossy(value).into_owned()
}
