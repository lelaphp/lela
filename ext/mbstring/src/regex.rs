//! `php_mbregex.c`: the `mb_ereg*` natives over oniguruma, the pattern
//! cache and the search state a request keeps, and the regex encoding,
//! which follows the internal encoding's ini entry.

use std::sync::Arc;

use crate::convert::check;
use crate::ffi::mbfl::Encoding;
use crate::ffi::onig::{
    self, MISMATCH, OnigEncodingASCII, OnigEncodingBIG5, OnigEncodingEUC_CN, OnigEncodingEUC_JP,
    OnigEncodingEUC_KR, OnigEncodingEUC_TW, OnigEncodingISO_8859_1, OnigEncodingISO_8859_2,
    OnigEncodingISO_8859_3, OnigEncodingISO_8859_4, OnigEncodingISO_8859_5, OnigEncodingISO_8859_6,
    OnigEncodingISO_8859_7, OnigEncodingISO_8859_8, OnigEncodingISO_8859_9,
    OnigEncodingISO_8859_10, OnigEncodingISO_8859_11, OnigEncodingISO_8859_13,
    OnigEncodingISO_8859_14, OnigEncodingISO_8859_15, OnigEncodingISO_8859_16, OnigEncodingKOI8_R,
    OnigEncodingSJIS, OnigEncodingUTF8, OnigEncodingUTF16_BE, OnigEncodingUTF16_LE,
    OnigEncodingUTF32_BE, OnigEncodingUTF32_LE, OnigSyntaxEmacs, OnigSyntaxGnuRegex,
    OnigSyntaxGrep, OnigSyntaxJava, OnigSyntaxPerl, OnigSyntaxPosixBasic, OnigSyntaxPosixExtended,
    OnigSyntaxRuby, Region, Table,
};
use crate::settings::settings;
use crate::text::{must_not_be_empty, new_string, value_error};
use php_ext::abi::{Callable, OrFalse, Ref, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, docref, throw_error};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value};

/// `ONIG_OPTION_*`.
const IGNORECASE: u32 = 1;
const EXTEND: u32 = 2;
const MULTILINE: u32 = 4;
const SINGLELINE: u32 = 8;
const FIND_LONGEST: u32 = 16;
const FIND_NOT_EMPTY: u32 = 32;

/// The syntaxes `_php_mb_regex_init_options` names, each by its letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Syntax {
    Java,
    GnuRegex,
    Grep,
    Emacs,
    Ruby,
    Perl,
    PosixBasic,
    PosixExtended,
}

impl Syntax {
    const ALL: [(u8, Syntax); 8] = [
        (b'j', Syntax::Java),
        (b'u', Syntax::GnuRegex),
        (b'g', Syntax::Grep),
        (b'c', Syntax::Emacs),
        (b'r', Syntax::Ruby),
        (b'z', Syntax::Perl),
        (b'b', Syntax::PosixBasic),
        (b'd', Syntax::PosixExtended),
    ];

    fn letter(self) -> u8 {
        Syntax::ALL
            .iter()
            .find(|(_, s)| *s == self)
            .map_or(b'r', |(c, _)| *c)
    }

    fn table(self) -> Table {
        match self {
            Syntax::Java => &OnigSyntaxJava,
            Syntax::GnuRegex => &OnigSyntaxGnuRegex,
            Syntax::Grep => &OnigSyntaxGrep,
            Syntax::Emacs => &OnigSyntaxEmacs,
            Syntax::Ruby => &OnigSyntaxRuby,
            Syntax::Perl => &OnigSyntaxPerl,
            Syntax::PosixBasic => &OnigSyntaxPosixBasic,
            Syntax::PosixExtended => &OnigSyntaxPosixExtended,
        }
    }
}

/// `enc_name_map`: oniguruma's encodings by the names php accepts,
/// the first the one `mb_regex_encoding()` answers. KOI8 is left out, as
/// php's configure leaves it out of a build whose oniguruma lacks it.
fn encodings() -> [(&'static [&'static str], Table); 28] {
    [
        (
            &["EUC-JP", "EUCJP", "X-EUC-JP", "UJIS", "EUCJP", "EUCJP-WIN"],
            &OnigEncodingEUC_JP,
        ),
        (&["UTF-8", "UTF8"], &OnigEncodingUTF8),
        (&["UTF-16", "UTF-16BE"], &OnigEncodingUTF16_BE),
        (&["UTF-16LE"], &OnigEncodingUTF16_LE),
        (&["UCS-4", "UTF-32", "UTF-32BE"], &OnigEncodingUTF32_BE),
        (&["UCS-4LE", "UTF-32LE"], &OnigEncodingUTF32_LE),
        (
            &[
                "SJIS",
                "CP932",
                "MS932",
                "SHIFT_JIS",
                "SJIS-WIN",
                "WINDOWS-31J",
            ],
            &OnigEncodingSJIS,
        ),
        (
            &["BIG5", "BIG-5", "BIGFIVE", "CN-BIG5", "BIG-FIVE"],
            &OnigEncodingBIG5,
        ),
        (
            &["EUC-CN", "EUCCN", "EUC_CN", "GB-2312", "GB2312"],
            &OnigEncodingEUC_CN,
        ),
        (&["EUC-TW", "EUCTW", "EUC_TW"], &OnigEncodingEUC_TW),
        (&["EUC-KR", "EUCKR", "EUC_KR"], &OnigEncodingEUC_KR),
        (&["KOI8R", "KOI8-R", "KOI-8R"], &OnigEncodingKOI8_R),
        (&["ISO-8859-1", "ISO8859-1"], &OnigEncodingISO_8859_1),
        (&["ISO-8859-2", "ISO8859-2"], &OnigEncodingISO_8859_2),
        (&["ISO-8859-3", "ISO8859-3"], &OnigEncodingISO_8859_3),
        (&["ISO-8859-4", "ISO8859-4"], &OnigEncodingISO_8859_4),
        (&["ISO-8859-5", "ISO8859-5"], &OnigEncodingISO_8859_5),
        (&["ISO-8859-6", "ISO8859-6"], &OnigEncodingISO_8859_6),
        (&["ISO-8859-7", "ISO8859-7"], &OnigEncodingISO_8859_7),
        (&["ISO-8859-8", "ISO8859-8"], &OnigEncodingISO_8859_8),
        (&["ISO-8859-9", "ISO8859-9"], &OnigEncodingISO_8859_9),
        (&["ISO-8859-10", "ISO8859-10"], &OnigEncodingISO_8859_10),
        (&["ISO-8859-11", "ISO8859-11"], &OnigEncodingISO_8859_11),
        (&["ISO-8859-13", "ISO8859-13"], &OnigEncodingISO_8859_13),
        (&["ISO-8859-14", "ISO8859-14"], &OnigEncodingISO_8859_14),
        (&["ISO-8859-15", "ISO8859-15"], &OnigEncodingISO_8859_15),
        (&["ISO-8859-16", "ISO8859-16"], &OnigEncodingISO_8859_16),
        (
            &["ASCII", "US-ASCII", "US_ASCII", "ISO646"],
            &OnigEncodingASCII,
        ),
    ]
}

/// The regex encoding: oniguruma's, and libmbfl's of the same name,
/// which checks a subject before a search.
#[derive(Clone, Copy)]
struct Mbctype {
    onig: Table,
    mbfl: Encoding,
}

impl Mbctype {
    /// `_php_mb_regex_name2mbctype`.
    fn onig(name: &[u8]) -> Option<Table> {
        if name.is_empty() {
            return None;
        }
        encodings()
            .into_iter()
            .find(|(names, _)| {
                names
                    .iter()
                    .any(|n| n.as_bytes().eq_ignore_ascii_case(name))
            })
            .map(|(_, table)| table)
    }

    /// `php_mb_regex_set_mbctype`'s lookup: a name both know.
    fn named(name: &[u8]) -> Option<Mbctype> {
        Some(Mbctype {
            onig: Mbctype::onig(name)?,
            mbfl: Encoding::named(name)?,
        })
    }

    /// `_php_mb_regex_mbctype2name`.
    fn name(self) -> &'static str {
        encodings()
            .into_iter()
            .find(|(_, table)| std::ptr::eq(*table, self.onig))
            .map_or("", |(names, _)| names[0])
    }

    fn utf8() -> Mbctype {
        Mbctype {
            onig: &OnigEncodingUTF8,
            mbfl: Encoding::utf8(),
        }
    }
}

/// A compiled pattern with what it was compiled under.
struct Compiled {
    regex: onig::Regex,
    options: u32,
    onig: Table,
    syntax: Syntax,
}

/// `MBREX`: the request's regex state.
pub(crate) struct State {
    /// `current_mbctype`, where `mb_regex_encoding()` or the internal
    /// encoding's entry set it; else the one the entry's value names.
    current: Option<Mbctype>,
    /// `regex_default_options` and `regex_default_syntax`.
    options: u32,
    syntax: Syntax,
    /// `ht_rc`.
    cache: Vec<(Vec<u8>, Arc<Compiled>)>,
    /// `search_str`, one owner held; `search_pos`; `search_re`;
    /// `search_regs`.
    search_str: Option<Owner<PhpString>>,
    search_pos: usize,
    search_re: Option<Arc<Compiled>>,
    search_regs: Option<Region>,
}

impl Default for State {
    fn default() -> State {
        State {
            current: None,
            options: MULTILINE | SINGLELINE,
            syntax: Syntax::Ruby,
            cache: Vec::new(),
            search_str: None,
            search_pos: 0,
            search_re: None,
            search_regs: None,
        }
    }
}

fn state(req: &mut Request) -> &mut State {
    &mut req.state::<crate::settings::State>().regex
}

/// The regex encoding: what was set this request, else the one the
/// internal encoding's name maps to, UTF-8 where it maps to none.
fn mbctype(req: &mut Request) -> Mbctype {
    if let Some(current) = state(req).current {
        return current;
    }
    let internal = settings(req).internal;
    Mbctype::named(internal.name()).unwrap_or_else(Mbctype::utf8)
}

/// `php_mb_regex_set_default_mbctype` and `php_mb_regex_set_mbctype`,
/// which `_php_mb_ini_mbstring_internal_encoding_set` calls with the
/// entry's value: the regex encoding follows where the name maps.
pub(crate) fn internal_encoding_changed(req: &mut Request, name: &[u8]) {
    if let Some(mbctype) = Mbctype::named(name) {
        state(req).current = Some(mbctype);
    }
}

/// `PHP_RSHUTDOWN_FUNCTION(mb_regex)`: the search string's owner given
/// back.
pub(crate) fn rshutdown(req: &mut Request) {
    if let Some(s) = state(req).search_str.take() {
        s.give_up(req);
    }
}

/// `PHP_MINIT_FUNCTION(mb_regex)`'s `onig_init`
/// (`Z/ext/mbstring/php_mbregex.c:115`), the process start (ADR-0010):
/// called once for the process, the first time an engine carrying
/// mbstring is built. php's `onig_end` at `MSHUTDOWN` has no
/// counterpart, since no host ends the process scope.
pub(crate) fn initialize() -> Initialized {
    onig::init();
    Initialized
}

/// The process scope's proof that oniguruma is started.
pub(crate) struct Initialized;

/// `PHP_MINFO_FUNCTION(mb_regex)`.
pub(crate) fn info(req: &mut Request) {
    req.extensions_mut().info_table(&[
        &["Multibyte (japanese) regex support", "enabled"],
        &["Multibyte regex (oniguruma) version", env!("ONIG_VERSION")],
    ]);
}

/// `mbstring.regex_stack_limit` and `mbstring.regex_retry_limit`, each
/// the quantity its `OnUpdateLong` read, where it fits an `unsigned int`.
///
/// A retry limit of `0x1` is one retry, and the match that needs more
/// fails:
///
/// ```php
/// var_dump(mb_ereg('(a|ab)*c', 'abababababc'), ini_set('mbstring.regex_retry_limit', '0x1'), mb_ereg('(a|ab)*c', 'abababababc'), ini_set('mbstring.regex_stack_limit', '1q'));
/// ```
/// ```expect
/// Warning: Invalid "mbstring.regex_stack_limit" setting. Invalid quantity "1q": unknown multiplier "q", interpreting as "1" for backwards compatibility in %s on line 2
/// bool(true)
/// string(7) "1000000"
/// bool(false)
/// string(6) "100000"
/// ```
fn limits(req: &Request) -> (Option<u32>, Option<u32>) {
    let read = |name: &[u8]| -> Option<u32> { u32::try_from(req.ini_long(name)).ok() };
    (
        read(b"mbstring.regex_stack_limit"),
        read(b"mbstring.regex_retry_limit"),
    )
}

/// `php_mbregex_compile_pattern`: the pattern checked against the regex
/// encoding, then taken from the cache or compiled into it.
fn compile(
    req: &mut Request,
    pattern: &[u8],
    options: u32,
    syntax: Syntax,
) -> Option<Arc<Compiled>> {
    let enc = mbctype(req);
    if !check(pattern, enc.mbfl) {
        docref(
            req,
            Level::Warning,
            &format!("Pattern is not valid under {} encoding", enc.name()),
        );
        return None;
    }
    let st = state(req);
    let at = st.cache.iter().position(|(p, _)| p == pattern);
    if let Some(at) = at {
        let c = &st.cache[at].1;
        if c.options == options && std::ptr::eq(c.onig, enc.onig) && c.syntax == syntax {
            return Some(Arc::clone(c));
        }
    }
    let regex = match onig::Regex::new(pattern, options, enc.onig, syntax.table()) {
        Ok(regex) => regex,
        Err(text) => {
            docref(req, Level::Warning, &format!("mbregex compile err: {text}"));
            return None;
        }
    };
    let compiled = Arc::new(Compiled {
        regex,
        options,
        onig: enc.onig,
        syntax,
    });
    let st = state(req);
    match at {
        Some(at) => {
            if st
                .search_re
                .as_ref()
                .is_some_and(|re| Arc::ptr_eq(re, &st.cache[at].1))
            {
                st.search_re = None;
            }
            st.cache[at].1 = Arc::clone(&compiled);
        }
        None => st.cache.push((pattern.to_vec(), Arc::clone(&compiled))),
    }
    Some(compiled)
}

/// `_php_mb_regex_init_options`: the option letters, a letter it does
/// not know a `ValueError`.
fn parse_options(req: &mut Request, text: &[u8]) -> Result<(u32, Syntax), Thrown> {
    let mut options = 0;
    let mut syntax = Syntax::Ruby;
    for &c in text {
        match c {
            b'i' => options |= IGNORECASE,
            b'x' => options |= EXTEND,
            b'm' => options |= MULTILINE,
            b's' => options |= SINGLELINE,
            b'p' => options |= MULTILINE | SINGLELINE,
            b'l' => options |= FIND_LONGEST,
            b'n' => options |= FIND_NOT_EMPTY,
            c => {
                let Some((_, s)) = Syntax::ALL.iter().find(|(l, _)| *l == c) else {
                    let text = format!("Option \"{}\" is not supported", char::from(c));
                    return Err(throw_error(req, "ValueError", &text));
                };
                syntax = *s;
            }
        }
    }
    Ok((options, syntax))
}

/// The options a call reads: its own, or the request's defaults.
fn options_of(
    req: &mut Request,
    given: Option<PhpString>,
    base: u32,
) -> Result<(u32, Syntax), Thrown> {
    if let Some(text) = given {
        return parse_options(req, text.as_bytes()).map(|(o, s)| (base | o, s));
    }
    let st = state(req);
    Ok((base | st.options, st.syntax))
}

/// `_php_mb_regex_get_option_string`.
fn option_string(options: u32, syntax: Syntax) -> String {
    let mut out = String::new();
    if options & IGNORECASE != 0 {
        out.push('i');
    }
    if options & EXTEND != 0 {
        out.push('x');
    }
    if options & (MULTILINE | SINGLELINE) == MULTILINE | SINGLELINE {
        out.push('p');
    } else {
        if options & MULTILINE != 0 {
            out.push('m');
        }
        if options & SINGLELINE != 0 {
            out.push('s');
        }
    }
    if options & FIND_LONGEST != 0 {
        out.push('l');
    }
    if options & FIND_NOT_EMPTY != 0 {
        out.push('n');
    }
    out.push(char::from(syntax.letter()));
    out
}

/// The groups of a match as `$matches` holds them: each by number, a
/// group that took no part `false`, then each named one by its name
/// (`mb_regex_groups_iter`); `empty_ok` keeps a group that matched the
/// empty string, as the search natives keep it and `mb_ereg` does not.
fn groups(
    req: &mut Request,
    re: &Compiled,
    region: &Region,
    subject: &[u8],
    empty_ok: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    let all = region.groups();
    let slice = |beg: i32, end: i32| -> Option<(usize, usize)> {
        let (b, e) = (usize::try_from(beg).ok()?, usize::try_from(end).ok()?);
        ((if empty_ok { b <= e } else { b < e }) && e <= subject.len()).then_some((b, e))
    };
    for (i, &(beg, end)) in all.iter().enumerate() {
        let value = match slice(beg, end) {
            Some((b, e)) => new_string(req, &subject[b..e])?.into_value(),
            None => Owner::bool(false),
        };
        let at = Key::Int(i64::try_from(i).unwrap_or(0));
        out.set(req, Owner::take(req, at), value)?;
    }
    for (name, group) in re.regex.names(region) {
        let (beg, end) = usize::try_from(group)
            .ok()
            .and_then(|g| all.get(g).copied())
            .unwrap_or((-1, -1));
        let named = usize::try_from(beg)
            .ok()
            .zip(usize::try_from(end).ok())
            .filter(|(b, e)| b < e && *e <= subject.len());
        let value = match named {
            Some((b, e)) => new_string(req, &subject[b..e])?.into_value(),
            None => Owner::bool(false),
        };
        out.set_symtable_str(req, &name, value)?;
    }
    Ok(out)
}

/// `_php_mb_regex_ereg_exec`.
fn ereg(
    req: &mut Request,
    pattern: PhpString,
    string: PhpString,
    matches: Ref<Untyped<Value>>,
    icase: bool,
) -> Result<bool, Thrown> {
    if pattern.is_empty() {
        return Err(must_not_be_empty(req, 1));
    }
    if matches.is_given() {
        let empty = PhpArray::new(req)?;
        matches.set(req, Untyped(empty.into_value()))?;
    }
    let enc = mbctype(req);
    if !check(string.as_bytes(), enc.mbfl) {
        return Ok(false);
    }
    let options = state(req).options | if icase { IGNORECASE } else { 0 };
    let syntax = state(req).syntax;
    let Some(re) = compile(req, pattern.as_bytes(), options, syntax) else {
        return Ok(false);
    };
    let Some(mut region) = Region::new() else {
        return Ok(false);
    };
    let subject = string.as_bytes();
    if re.regex.search(subject, 0, &mut region, limits(req)) < 0 {
        return Ok(false);
    }
    if matches.is_given() {
        let found = groups(req, &re, &region, subject, false)?;
        matches.set(req, Untyped(found.into_value()))?;
    }
    Ok(true)
}

/// `mb_ereg(string $pattern, string $string, &$matches = null): bool`.
///
/// ```php
/// var_dump(mb_ereg("(ñ)(x)?", "añb", $m), $m, mb_ereg("z", "abc"), mb_ereg("(?<w>b)", "abc", $n), $n);
/// try { mb_ereg("", "a"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// array(3) {
///   [0]=>
///   string(2) "ñ"
///   [1]=>
///   string(2) "ñ"
///   [2]=>
///   bool(false)
/// }
/// bool(false)
/// bool(true)
/// array(3) {
///   [0]=>
///   string(1) "b"
///   [1]=>
///   string(1) "b"
///   ["w"]=>
///   string(1) "b"
/// }
/// mb_ereg(): Argument #1 ($pattern) must not be empty
/// ```
#[php_function]
pub(crate) fn mb_ereg(
    req: &mut Request,
    pattern: PhpString,
    string: PhpString,
    #[php(default = null)] matches: Ref<Untyped<Value>>,
) -> Result<bool, Thrown> {
    ereg(req, pattern, string, matches, false)
}

/// `mb_eregi(string $pattern, string $string, &$matches = null): bool`.
///
/// ```php
/// var_dump(mb_eregi("Ñ", "añb", $m), $m, mb_eregi("Z", "abc"));
/// ```
/// ```expect
/// bool(true)
/// array(1) {
///   [0]=>
///   string(2) "ñ"
/// }
/// bool(false)
/// ```
#[php_function]
pub(crate) fn mb_eregi(
    req: &mut Request,
    pattern: PhpString,
    string: PhpString,
    #[php(default = null)] matches: Ref<Untyped<Value>>,
) -> Result<bool, Thrown> {
    ereg(req, pattern, string, matches, true)
}

/// `php_mb_mbchar_bytes`: the bytes the character at `s` takes.
fn char_bytes(s: &[u8], enc: Encoding) -> usize {
    if let Some(table) = enc.mblen_table() {
        return s.first().map_or(1, |&b| usize::from(table[usize::from(b)]));
    }
    match enc.flag() {
        f if f & crate::tables::MBFL_ENCTYPE_WCS2 != 0 => 2,
        f if f & crate::tables::MBFL_ENCTYPE_WCS4 != 0 => 4,
        _ => 1,
    }
}

/// `mb_regex_substitute`: `\0` to `\9` and `\k<name>` in `replace` with
/// what those groups matched; anything else as it is.
#[allow(clippy::too_many_lines)] // one C function, kept whole
fn substitute(
    out: &mut Vec<u8>,
    subject: &[u8],
    replace: &[u8],
    re: &Compiled,
    region: &Region,
    enc: Encoding,
) {
    let groups = region.groups();
    let mut p = 0usize;
    let eos = replace.len();
    while p < eos {
        let clen = char_bytes(&replace[p..], enc);
        if clen != 1 || replace[p] != b'\\' {
            out.extend_from_slice(&replace[p..(p + clen).min(eos)]);
            p += clen;
            continue;
        }
        let sp = p;
        p += 1;
        let clen = char_bytes(&replace[p.min(eos)..], enc);
        if clen != 1 || p == eos {
            out.extend_from_slice(&replace[sp..p.min(eos)]);
            continue;
        }
        let no: i32;
        match replace[p] {
            b'0' => {
                no = 0;
                p += 1;
            }
            c @ b'1'..=b'9' => {
                if !re.regex.numbered_groups() {
                    p += 1;
                    out.extend_from_slice(&replace[sp..p]);
                    continue;
                }
                no = i32::from(c - b'0');
                p += 1;
            }
            b'k' => {
                p += 1;
                let clen = char_bytes(&replace[p.min(eos)..], enc);
                if clen != 1 || p == eos || (replace[p] != b'<' && replace[p] != b'\'') {
                    p = (p + clen).min(eos);
                    out.extend_from_slice(&replace[sp..p]);
                    continue;
                }
                let delim = if replace[p] == b'<' { b'>' } else { b'\'' };
                let name = p + 1;
                let mut name_end = name;
                let mut maybe_num = true;
                while name_end < eos {
                    let clen = char_bytes(&replace[name_end..], enc);
                    if clen != 1 {
                        name_end += clen;
                        maybe_num = false;
                        continue;
                    }
                    if replace[name_end] == delim {
                        break;
                    }
                    if maybe_num && !replace[name_end].is_ascii_digit() {
                        maybe_num = false;
                    }
                    name_end += 1;
                }
                p = name_end + 1;
                if name_end <= name || name_end >= eos {
                    out.extend_from_slice(&replace[sp..p.min(eos)]);
                    continue;
                }
                let digits = &replace[name..name_end];
                if maybe_num {
                    if !re.regex.numbered_groups() {
                        out.extend_from_slice(&replace[sp..p]);
                        continue;
                    }
                    no = if digits.len() == 1 {
                        i32::from(digits[0] - b'0')
                    } else if digits[0] == b'0' {
                        -1
                    } else {
                        std::str::from_utf8(digits)
                            .ok()
                            .and_then(|d| d.parse::<u64>().ok())
                            .map_or(i32::MAX, |n| i32::try_from(n).unwrap_or(-1))
                    };
                } else {
                    no = re.regex.backref(digits, region);
                }
            }
            _ => {
                out.extend_from_slice(&replace[sp..p]);
                continue;
            }
        }
        let group = usize::try_from(no).ok().and_then(|n| groups.get(n));
        let Some(&(beg, end)) = group else {
            out.extend_from_slice(&replace[sp..p]);
            continue;
        };
        if let (Ok(b), Ok(e)) = (usize::try_from(beg), usize::try_from(end))
            && b < e
            && e <= subject.len()
        {
            out.extend_from_slice(&subject[b..e]);
        }
    }
}

/// What replaces each match: text with group references, or a callback.
enum Replacement<'a> {
    Text(&'a [u8]),
    Callback(&'a Callable),
}

/// `_php_mb_regex_ereg_replace_exec`.
fn replace(
    req: &mut Request,
    pattern: PhpString,
    replacement: &Replacement<'_>,
    string: PhpString,
    options: Option<PhpString>,
    base: u32,
) -> Result<Option<OrFalse<Owner<PhpString>>>, Thrown> {
    let enc = mbctype(req);
    if !check(string.as_bytes(), enc.mbfl) {
        return Ok(None);
    }
    let (options, syntax) = options_of(req, options, base)?;
    let Some(re) = compile(req, pattern.as_bytes(), options, syntax) else {
        return Ok(Some(OrFalse::False));
    };
    let subject = string.as_bytes();
    let mut out = Vec::with_capacity(subject.len());
    let mut matched = false;
    let mut pos = 0usize;
    let limits = limits(req);
    loop {
        let Some(mut region) = Region::new() else {
            break;
        };
        let err = re.regex.search(subject, pos, &mut region, limits);
        if err <= -2 {
            let text = onig::error_text(err);
            docref(
                req,
                Level::Warning,
                &format!("mbregex search failure in php_mbereg_replace_exec(): {text}"),
            );
            return Ok(Some(OrFalse::False));
        }
        if err < 0 {
            out.extend_from_slice(subject.get(pos..).unwrap_or_default());
            break;
        }
        matched = true;
        let g = region.groups();
        let (beg0, end0) = g.first().copied().unwrap_or((0, 0));
        let beg0 = usize::try_from(beg0).unwrap_or(pos);
        out.extend_from_slice(subject.get(pos..beg0).unwrap_or_default());
        match replacement {
            Replacement::Text(text) => substitute(&mut out, subject, text, &re, &region, enc.mbfl),
            Replacement::Callback(callback) => {
                let mut subpats = PhpArray::new(req)?;
                for &(beg, end) in &g {
                    let (b, e) = (
                        usize::try_from(beg).unwrap_or(0),
                        usize::try_from(end).unwrap_or(0),
                    );
                    let s = new_string(req, subject.get(b..e).unwrap_or_default())?;
                    subpats.append(req, s.into_value())?;
                }
                for (name, group) in re.regex.names(&region) {
                    let (beg, end) = usize::try_from(group)
                        .ok()
                        .and_then(|n| g.get(n).copied())
                        .unwrap_or((-1, -1));
                    let named = usize::try_from(beg)
                        .ok()
                        .zip(usize::try_from(end).ok())
                        .filter(|(b, e)| b < e && *e <= subject.len());
                    let value = match named {
                        Some((b, e)) => new_string(req, &subject[b..e])?.into_value(),
                        None => Owner::bool(false),
                    };
                    subpats.set_symtable_str(req, &name, value)?;
                }
                let returned = php_ext::call::call(req, callback, &[Value::array(*subpats)]);
                subpats.give_up(req);
                let returned = returned?;
                let read = php_ext::coerce::with_string(req, &returned, |text| {
                    out.extend_from_slice(text);
                });
                returned.give_up(req);
                read?;
            }
        }
        let n = usize::try_from(end0).unwrap_or(pos);
        if pos < n {
            pos = n;
        } else {
            if pos < subject.len() {
                out.push(subject[pos]);
            }
            pos += 1;
        }
    }
    // `smart_str_extract`: a buffer nothing was appended to, no match
    // over an empty subject, is `ZSTR_EMPTY_ALLOC()`; one a match or the
    // tail started is the request's, though it holds nothing.
    let answer = if matched || !out.is_empty() {
        new_string(req, &out)?
    } else {
        crate::text::cut_string(req, b"")?
    };
    Ok(Some(OrFalse::Value(answer)))
}

/// `mb_ereg_replace(string $pattern, string $replacement, string $string, ?string $options = null): string|false|null`.
///
/// ```php
/// var_dump(mb_ereg_replace("ñ", "n", "añbñ"), mb_ereg_replace("(a)(b)", "\\2\\1", "abab"), mb_ereg_replace("(?<x>b)", "[\\k<x>]", "abc"), mb_ereg_replace("", "-", "ab"), mb_ereg_replace("a", "b", "\xff"));
/// try { mb_ereg_replace("a", "b", "c", "q"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "anbn"
/// string(4) "baba"
/// string(5) "a[b]c"
/// string(5) "-a-b-"
/// NULL
/// Option "q" is not supported
/// ```
#[php_function]
pub(crate) fn mb_ereg_replace(
    req: &mut Request,
    pattern: PhpString,
    replacement: PhpString,
    string: PhpString,
    #[php(default = null)] options: Option<PhpString>,
) -> Result<Option<OrFalse<Owner<PhpString>>>, Thrown> {
    replace(
        req,
        pattern,
        &Replacement::Text(replacement.as_bytes()),
        string,
        options,
        0,
    )
}

/// `mb_eregi_replace(string $pattern, string $replacement, string $string, ?string $options = null): string|false|null`.
///
/// ```php
/// var_dump(mb_eregi_replace("Ñ", "n", "añbÑ"));
/// ```
/// ```expect
/// string(4) "anbn"
/// ```
#[php_function]
pub(crate) fn mb_eregi_replace(
    req: &mut Request,
    pattern: PhpString,
    replacement: PhpString,
    string: PhpString,
    #[php(default = null)] options: Option<PhpString>,
) -> Result<Option<OrFalse<Owner<PhpString>>>, Thrown> {
    replace(
        req,
        pattern,
        &Replacement::Text(replacement.as_bytes()),
        string,
        options,
        IGNORECASE,
    )
}

/// `mb_ereg_replace_callback(string $pattern, callable $callback, string $string, ?string $options = null): string|false|null`.
///
/// ```php
/// var_dump(mb_ereg_replace_callback("(ñ)", fn($m) => "<$m[1]>", "añbñ"), mb_ereg_replace_callback("(?<d>\\d)", fn($m) => $m["d"] * 2, "a1b2"));
/// ```
/// ```expect
/// string(10) "a<ñ>b<ñ>"
/// string(4) "a2b4"
/// ```
#[php_function]
pub(crate) fn mb_ereg_replace_callback(
    req: &mut Request,
    pattern: PhpString,
    callback: Callable,
    string: PhpString,
    #[php(default = null)] options: Option<PhpString>,
) -> Result<Option<OrFalse<Owner<PhpString>>>, Thrown> {
    replace(
        req,
        pattern,
        &Replacement::Callback(&callback),
        string,
        options,
        0,
    )
}

/// `mb_split(string $pattern, string $string, int $limit = -1): array|false`.
///
/// ```php
/// var_dump(mb_split("ñ", "añbñc"), mb_split("ñ", "añbñc", 2), mb_split("x", ""));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
///   [2]=>
///   string(1) "c"
/// }
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(4) "bñc"
/// }
/// array(1) {
///   [0]=>
///   string(0) ""
/// }
/// ```
#[php_function]
pub(crate) fn mb_split(
    req: &mut Request,
    pattern: PhpString,
    string: PhpString,
    #[php(default = -1)] limit: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    // `zend_ulong count`: the limit read unsigned, so a negative one is
    // all but unlimited, and one fewer than asked split off.
    let mut count = limit.cast_unsigned().saturating_sub(1);
    let enc = mbctype(req);
    if !check(string.as_bytes(), enc.mbfl) {
        return Ok(OrFalse::False);
    }
    let (options, syntax) = {
        let st = state(req);
        (st.options, st.syntax)
    };
    let Some(re) = compile(req, pattern.as_bytes(), options, syntax) else {
        return Ok(OrFalse::False);
    };
    let subject = string.as_bytes();
    let mut out = PhpArray::new(req)?;
    let (mut pos, mut chunk) = (0usize, 0usize);
    let limits = limits(req);
    let mut failed = None;
    while count != 0 && pos < subject.len() {
        let Some(mut region) = Region::new() else {
            break;
        };
        let err = re.regex.search(subject, pos, &mut region, limits);
        if err < 0 {
            if err <= -2 {
                failed = Some(err);
            }
            break;
        }
        let (beg, end) = region.groups().first().copied().unwrap_or((0, 0));
        let (beg, end) = (
            usize::try_from(beg).unwrap_or(0),
            usize::try_from(end).unwrap_or(0),
        );
        if pos < end {
            if beg < subject.len() && beg >= chunk {
                let s = new_string(req, &subject[chunk..beg])?;
                out.append(req, s.into_value())?;
                count = count.wrapping_sub(1);
            } else {
                failed = Some(-2);
                break;
            }
            pos = end;
            chunk = end;
        } else {
            pos += 1;
        }
    }
    if let Some(err) = failed {
        let text = onig::error_text(err);
        docref(
            req,
            Level::Warning,
            &format!("mbregex search failure in mbsplit(): {text}"),
        );
        out.give_up(req);
        return Ok(OrFalse::False);
    }
    let tail = new_string(req, subject.get(chunk..).unwrap_or_default())?;
    out.append(req, tail.into_value())?;
    Ok(OrFalse::Value(out))
}

/// `mb_ereg_match(string $pattern, string $string, ?string $options = null): bool`.
///
/// ```php
/// var_dump(mb_ereg_match("a", "abc"), mb_ereg_match("b", "abc"), mb_ereg_match(".*ñ", "añ"));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
pub(crate) fn mb_ereg_match(
    req: &mut Request,
    pattern: PhpString,
    string: PhpString,
    #[php(default = null)] options: Option<PhpString>,
) -> Result<bool, Thrown> {
    let (options, syntax) = options_of(req, options, 0)?;
    let enc = mbctype(req);
    if !check(string.as_bytes(), enc.mbfl) {
        return Ok(false);
    }
    let Some(re) = compile(req, pattern.as_bytes(), options, syntax) else {
        return Ok(false);
    };
    let (stack, retry) = limits(req);
    let positive = |n: Option<u32>| n.filter(|&n| n > 0 && n < u32::MAX);
    Ok(re
        .regex
        .matches_at_start(string.as_bytes(), (positive(stack), positive(retry)))
        >= 0)
}

/// The options a search native reads: its own, or the request's
/// defaults.
fn search_options(req: &mut Request, given: Option<PhpString>) -> Result<(u32, Syntax), Thrown> {
    if let Some(text) = given {
        return parse_options(req, text.as_bytes());
    }
    let st = state(req);
    Ok((st.options, st.syntax))
}

/// `_php_mb_regex_ereg_search_exec`: a search from the search position
/// with the given pattern or the last, its groups kept for
/// `mb_ereg_search_getregs()` and the position moved past the match;
/// `false` where it found none or the pattern would not compile.
fn search(
    req: &mut Request,
    pattern: Option<PhpString>,
    options: Option<PhpString>,
) -> Result<bool, Thrown> {
    let (options, syntax) = search_options(req, options)?;
    state(req).search_regs = None;
    if let Some(pattern) = pattern {
        let compiled = compile(req, pattern.as_bytes(), options, syntax);
        let failed = compiled.is_none();
        state(req).search_re = compiled;
        if failed {
            return Ok(false);
        }
    }
    let st = state(req);
    let (re, pos, subject) = (
        st.search_re.clone(),
        st.search_pos,
        st.search_str.as_deref().copied(),
    );
    let Some(re) = re else {
        return Err(throw_error(req, "Error", "No pattern was provided"));
    };
    let Some(subject) = subject else {
        return Err(throw_error(req, "Error", "No string was provided"));
    };
    let Some(mut region) = Region::new() else {
        return Ok(false);
    };
    let bytes = subject.as_bytes();
    let err = re.regex.search(bytes, pos, &mut region, limits(req));
    if err == MISMATCH {
        state(req).search_pos = bytes.len();
        return Ok(false);
    }
    if err <= -2 {
        let text = onig::error_text(err);
        docref(
            req,
            Level::Warning,
            &format!("mbregex search failure in mbregex_search(): {text}"),
        );
        return Ok(false);
    }
    let end = region
        .groups()
        .first()
        .map_or(0, |&(_, end)| usize::try_from(end).unwrap_or(0));
    let st = state(req);
    st.search_pos = if pos <= end { end } else { pos + 1 };
    st.search_regs = Some(region);
    Ok(true)
}

/// `mb_ereg_search(?string $pattern = null, ?string $options = null): bool`.
///
/// ```php
/// mb_ereg_search_init("añbñ", "ñ"); var_dump(mb_ereg_search(), mb_ereg_search_getpos(), mb_ereg_search(), mb_ereg_search());
/// ```
/// ```expect
/// bool(true)
/// int(3)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
pub(crate) fn mb_ereg_search(
    req: &mut Request,
    #[php(default = null)] pattern: Option<PhpString>,
    #[php(default = null)] options: Option<PhpString>,
) -> Result<bool, Thrown> {
    search(req, pattern, options)
}

/// `mb_ereg_search_pos(?string $pattern = null, ?string $options = null): array|false`.
///
/// ```php
/// mb_ereg_search_init("añbñ"); var_dump(mb_ereg_search_pos("ñ"));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// ```
#[php_function]
pub(crate) fn mb_ereg_search_pos(
    req: &mut Request,
    #[php(default = null)] pattern: Option<PhpString>,
    #[php(default = null)] options: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    if !search(req, pattern, options)? {
        return Ok(OrFalse::False);
    }
    let (beg, end) = state(req)
        .search_regs
        .as_ref()
        .and_then(|region| region.groups().first().copied())
        .unwrap_or((0, 0));
    let mut out = PhpArray::new(req)?;
    out.append(req, Owner::int(i64::from(beg)))?;
    out.append(req, Owner::int(i64::from(end - beg)))?;
    Ok(OrFalse::Value(out))
}

/// `mb_ereg_search_regs(?string $pattern = null, ?string $options = null): array|false`.
///
/// ```php
/// mb_ereg_search_init("añbñ"); var_dump(mb_ereg_search_regs("(ñ)(x)?"));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   string(2) "ñ"
///   [1]=>
///   string(2) "ñ"
///   [2]=>
///   bool(false)
/// }
/// ```
#[php_function]
pub(crate) fn mb_ereg_search_regs(
    req: &mut Request,
    #[php(default = null)] pattern: Option<PhpString>,
    #[php(default = null)] options: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    if !search(req, pattern, options)? {
        return Ok(OrFalse::False);
    }
    mb_ereg_search_getregs(req)
}

/// `mb_ereg_search_init(string $string, ?string $pattern = null, ?string $options = null): bool`.
///
/// ```php
/// var_dump(mb_ereg_search_init("abc"), mb_ereg_search_init("a\xff"), mb_ereg_search_getpos());
/// try { mb_ereg_search_init("a", ""); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// int(2)
/// mb_ereg_search_init(): Argument #2 ($pattern) must not be empty
/// ```
#[php_function]
pub(crate) fn mb_ereg_search_init(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] pattern: Option<PhpString>,
    #[php(default = null)] options: Option<PhpString>,
) -> Result<bool, Thrown> {
    if pattern.as_ref().is_some_and(PhpString::is_empty) {
        return Err(must_not_be_empty(req, 2));
    }
    let (options, syntax) = search_options(req, options)?;
    if let Some(pattern) = pattern {
        let compiled = compile(req, pattern.as_bytes(), options, syntax);
        let failed = compiled.is_none();
        state(req).search_re = compiled;
        if failed {
            return Ok(false);
        }
    }
    let held = Owner::take(req, string);
    if let Some(old) = state(req).search_str.replace(held) {
        old.give_up(req);
    }
    let enc = mbctype(req);
    let valid = check(string.as_bytes(), enc.mbfl);
    let st = state(req);
    st.search_pos = if valid { 0 } else { string.len() };
    st.search_regs = None;
    Ok(valid)
}

/// `mb_ereg_search_getregs(): array|false`.
///
/// ```php
/// var_dump(mb_ereg_search_getregs()); mb_ereg_search_init("abc", "(?<x>b)"); mb_ereg_search(); var_dump(mb_ereg_search_getregs());
/// ```
/// ```expect
/// bool(false)
/// array(3) {
///   [0]=>
///   string(1) "b"
///   [1]=>
///   string(1) "b"
///   ["x"]=>
///   string(1) "b"
/// }
/// ```
#[php_function]
pub(crate) fn mb_ereg_search_getregs(
    req: &mut Request,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let st = state(req);
    let (Some(region), Some(subject), Some(re)) = (
        st.search_regs.take(),
        st.search_str.as_deref().copied(),
        st.search_re.clone(),
    ) else {
        return Ok(OrFalse::False);
    };
    let found = groups(req, &re, &region, subject.as_bytes(), true);
    state(req).search_regs = Some(region);
    found.map(OrFalse::Value)
}

/// `mb_ereg_search_getpos(): int`.
///
/// ```php
/// var_dump(mb_ereg_search_getpos()); mb_ereg_search_init("abc"); mb_ereg_search_setpos(2); var_dump(mb_ereg_search_getpos());
/// ```
/// ```expect
/// int(0)
/// int(2)
/// ```
#[php_function]
pub(crate) fn mb_ereg_search_getpos(req: &mut Request) -> i64 {
    i64::try_from(state(req).search_pos).unwrap_or(i64::MAX)
}

/// `mb_ereg_search_setpos(int $offset): bool`.
///
/// ```php
/// mb_ereg_search_init("abc"); var_dump(mb_ereg_search_setpos(-1), mb_ereg_search_getpos());
/// try { mb_ereg_search_setpos(9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(2)
/// mb_ereg_search_setpos(): Argument #1 ($offset) is out of range
/// ```
#[php_function]
pub(crate) fn mb_ereg_search_setpos(req: &mut Request, offset: i64) -> Result<bool, Thrown> {
    let len = state(req)
        .search_str
        .as_deref()
        .map(|s| i64::try_from(s.len()).unwrap_or(i64::MAX));
    let mut position = offset;
    if position < 0
        && let Some(len) = len
    {
        position += len;
    }
    if position < 0 || len.is_some_and(|len| position > len) {
        return Err(value_error(req, 1, "is out of range"));
    }
    state(req).search_pos = usize::try_from(position).unwrap_or(0);
    Ok(true)
}

/// `mb_regex_set_options(?string $options = null): string`: the options
/// as they stood.
///
/// ```php
/// var_dump(mb_regex_set_options(), mb_regex_set_options("ix"), mb_regex_set_options());
/// ```
/// ```expect
/// string(2) "pr"
/// string(2) "pr"
/// string(3) "ixr"
/// ```
#[php_function]
pub(crate) fn mb_regex_set_options(
    req: &mut Request,
    #[php(default = null)] options: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let (previous, previous_syntax) = {
        let st = state(req);
        (st.options, st.syntax)
    };
    if let Some(text) = options {
        let (options, syntax) = parse_options(req, text.as_bytes())?;
        let st = state(req);
        st.options = options;
        st.syntax = syntax;
    }
    new_string(req, option_string(previous, previous_syntax).as_bytes())
}

/// `mb_regex_encoding(?string $encoding = null): string|bool`.
///
/// ```php
/// var_dump(mb_regex_encoding(), mb_regex_encoding("sjis"), mb_regex_encoding());
/// try { mb_regex_encoding("UTF-7"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(5) "UTF-8"
/// bool(true)
/// string(4) "SJIS"
/// mb_regex_encoding(): Argument #1 ($encoding) must be a valid encoding, "UTF-7" given
/// ```
#[php_function]
pub(crate) fn mb_regex_encoding(
    req: &mut Request,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Union<Owner<PhpString>, bool>, Thrown> {
    let Some(name) = encoding else {
        let current = mbctype(req);
        return Ok(Union::A(new_string(req, current.name().as_bytes())?));
    };
    let shown = php_ext::coerce::c_str(name.as_bytes());
    let Some(mbctype) = Mbctype::named(shown) else {
        let text = String::from_utf8_lossy(shown).into_owned();
        return Err(value_error(
            req,
            1,
            &format!("must be a valid encoding, \"{text}\" given"),
        ));
    };
    state(req).current = Some(mbctype);
    Ok(Union::B(true))
}
