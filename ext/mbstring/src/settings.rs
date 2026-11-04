//! `MBSTRG`: what `mbstring.c` keeps per request, its ini entries and
//! their `PHP_INI_MH` handlers, the hook php's `main.c` serves when one of
//! its own encodings changes (`php_internal_encoding_changed`), and the
//! encoding arguments and lists every native reads.
//!
//! php keeps two layers, the ini's (`internal_encoding`,
//! `filter_illegal_mode`, ...) and the request's (`current_*`), which
//! `RINIT` copies down. Here both are the request's: [`Settings`] is
//! built from the ini on the request's first touch, which every handler
//! makes before it changes anything, so what php's `RINIT` read is what
//! is read.

use std::ops::ControlFlow;

use crate::ffi::mbfl::{Encoding, Language};
use crate::tables::{
    ENCODING_8859_9, ENCODING_ARMSCII8, ENCODING_ASCII, ENCODING_BIG5, ENCODING_CHARSET_MIN,
    ENCODING_CP866, ENCODING_CP936, ENCODING_CP1251, ENCODING_CP1254, ENCODING_EUC_CN,
    ENCODING_EUC_JP, ENCODING_EUC_KR, ENCODING_EUC_TW, ENCODING_JIS, ENCODING_KOI8R,
    ENCODING_KOI8U, ENCODING_SJIS, ENCODING_UHC, ENCODING_UTF8, LANGUAGE_ARMENIAN,
    LANGUAGE_JAPANESE, LANGUAGE_KOREAN, LANGUAGE_NEUTRAL, LANGUAGE_RUSSIAN,
    LANGUAGE_SIMPLIFIED_CHINESE, LANGUAGE_TRADITIONAL_CHINESE, LANGUAGE_TURKISH,
    LANGUAGE_UKRAINIAN, MBFL_OUTPUTFILTER_ILLEGAL_MODE_BADUTF8,
    MBFL_OUTPUTFILTER_ILLEGAL_MODE_CHAR, MBFL_OUTPUTFILTER_ILLEGAL_MODE_ENTITY,
    MBFL_OUTPUTFILTER_ILLEGAL_MODE_LONG, MBFL_OUTPUTFILTER_ILLEGAL_MODE_NONE,
};
use crate::text::value_error;
use php_ext::abi::IniStage;
use php_ext::coerce::is_space;
use php_ext::decl::{Bytes, IniDecl, Name};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Ini, Request};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Emalloc, Owner, PhpArray, PhpString};

/// `MBFL_OUTPUTFILTER_ILLEGAL_MODE_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    None,
    Char,
    Long,
    Entity,
    /// `BADUTF8`, which only the searches ask for: `0xFF` for an error,
    /// a byte no UTF-8 text holds.
    BadUtf8,
}

/// `filter_illegal_mode` with `filter_illegal_substchar`: how a
/// character the target cannot hold is written. The character is kept
/// whatever the kind, since `long` and `entity` still write it for a
/// byte sequence the source could not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Substitute {
    pub(crate) mode: Mode,
    pub(crate) char: u32,
}

impl Substitute {
    /// `PHP_GINIT_FUNCTION(mbstring)`'s: `?`.
    const DEFAULT: Substitute = Substitute {
        mode: Mode::Char,
        char: b'?' as u32,
    };

    /// What a search converts with.
    pub(crate) const BAD_UTF8: Substitute = Substitute {
        mode: Mode::BadUtf8,
        char: 0,
    };

    /// `'?'` in `MODE_CHAR`, which the MIME and mail paths use whatever
    /// the request's.
    pub(crate) const QUESTION: Substitute = Substitute::DEFAULT;

    /// The kind as libmbfl numbers it.
    pub(crate) fn raw_mode(self) -> u32 {
        match self.mode {
            Mode::None => MBFL_OUTPUTFILTER_ILLEGAL_MODE_NONE,
            Mode::Char => MBFL_OUTPUTFILTER_ILLEGAL_MODE_CHAR,
            Mode::Long => MBFL_OUTPUTFILTER_ILLEGAL_MODE_LONG,
            Mode::Entity => MBFL_OUTPUTFILTER_ILLEGAL_MODE_ENTITY,
            Mode::BadUtf8 => MBFL_OUTPUTFILTER_ILLEGAL_MODE_BADUTF8,
        }
    }

    /// A word that names a kind, ignoring case.
    pub(crate) fn named(word: &[u8]) -> Option<Mode> {
        [
            (&b"none"[..], Mode::None),
            (b"long", Mode::Long),
            (b"entity", Mode::Entity),
        ]
        .into_iter()
        .find_map(|(name, mode)| name.eq_ignore_ascii_case(word).then_some(mode))
    }

    /// `OnUpdate_mbstring_substitute_character` over `self`: a kind by
    /// its name, or else `Char`, whose character changes only where the
    /// whole value is a number as `strtol(.., 0)` reads one.
    fn updated(self, value: &[u8]) -> Substitute {
        if let Some(mode) = Substitute::named(value) {
            return Substitute { mode, ..self };
        }
        let char = strtol(value).map_or(self.char, i32::cast_unsigned);
        Substitute {
            mode: Mode::Char,
            char,
        }
    }

    /// What `mb_substitute_character()` and `mb_get_info()` answer: the
    /// kind's name, or the character for `Char`.
    pub(crate) fn shown(
        self,
        req: &mut Request,
    ) -> Result<php_ext::abi::Union<Owner<PhpString>, i64>, Thrown> {
        let name = match self.mode {
            Mode::Char | Mode::BadUtf8 => return Ok(php_ext::abi::Union::B(i64::from(self.char))),
            Mode::None => "none",
            Mode::Long => "long",
            Mode::Entity => "entity",
        };
        Ok(php_ext::abi::Union::A(PhpString::new(
            req,
            name.as_bytes(),
        )?))
    }
}

/// `strtol(s, &end, 0)` where `*end` must be the terminator: a whole
/// C integer, decimal, `0x` hex or `0` octal, with leading space and a
/// sign, cut to `int` as php's assignment cuts it. Kept beside the face's
/// `strtol`, which reads base 10 and answers no end; the space it skips
/// is the face's, C's `isspace` under the thread's `LC_CTYPE`.
fn strtol(text: &[u8]) -> Option<i32> {
    let start = text.iter().position(|b| !is_space(*b))?;
    let text = &text[start..];
    let (negative, digits) = match text.first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (radix, digits) =
        if let Some(hex) = digits.strip_prefix(b"0x").or(digits.strip_prefix(b"0X")) {
            (16, hex)
        } else if digits.len() > 1 && digits[0] == b'0' {
            (8, &digits[1..])
        } else {
            (10, digits)
        };
    if digits.is_empty() || !digits.iter().all(|&d| char::from(d).is_digit(radix)) {
        return None;
    }
    let n = digits
        .iter()
        .try_fold(0i64, |n, &d| {
            n.checked_mul(i64::from(radix))?
                .checked_add(i64::from(char::from(d).to_digit(radix)?))
        })
        .unwrap_or(i64::MAX);
    // The `long` assigned to an `int`: its low 32 bits.
    let n = if negative { n.wrapping_neg() } else { n };
    Some(i32::from_ne_bytes(n.to_ne_bytes()[..4].try_into().ok()?))
}

/// Where an input was identified, `http_input_identify` and its four
/// cousins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Get,
    Post,
    Cookie,
    String,
}

/// The request's `MBSTRG`.
pub(crate) struct Settings {
    pub(crate) language: Language,
    /// `default_detect_order_list`, the language's.
    pub(crate) default_detect: &'static [i32],
    /// `current_detect_order_list`.
    pub(crate) detect_order: Vec<Encoding>,
    /// `http_input_list`.
    pub(crate) http_input: Vec<Encoding>,
    /// `internal_encoding`, the ini's, which input translation writes in.
    pub(crate) internal: Encoding,
    /// `current_internal_encoding`.
    pub(crate) current_internal: Encoding,
    /// `current_http_output_encoding`.
    pub(crate) http_output: Encoding,
    /// `current_filter_illegal_mode` and `current_filter_illegal_substchar`.
    pub(crate) substitute: Substitute,
    pub(crate) strict_detection: bool,
    pub(crate) encoding_translation: bool,
    /// `internal_encoding_set`, `http_output_set` and `http_input_set`.
    internal_origin: Origin,
    http_output_origin: Origin,
    http_input_origin: Origin,
}

/// Where one of the three encodings came from, `internal_encoding_set`
/// and its two kin: php's own entry, which it follows as that entry
/// changes, or mbstring's entry or native, which a change of php's own
/// leaves alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Php,
    Own,
}

impl Origin {
    /// Set by mbstring's own entry where its value is not empty.
    fn of(value: &[u8]) -> Origin {
        if value.is_empty() {
            Origin::Php
        } else {
            Origin::Own
        }
    }
}

/// The extension's request state (ADR-0005).
#[derive(Default)]
pub(crate) struct State {
    pub(crate) settings: Option<Settings>,
    /// `illegalchars`.
    pub(crate) illegal_chars: u64,
    /// `http_input_identify` and, by where it came from, the four more.
    pub(crate) identified: Option<Encoding>,
    pub(crate) identified_from: [Option<Encoding>; 4],
    /// `last_used_encoding_name` and `last_used_encoding`.
    last_used: Option<(Vec<u8>, Encoding)>,
    /// `outconv_enabled` with `outconv_state`: the buffer handler's
    /// decoder state while it converts.
    pub(crate) outconv: Option<u32>,
    /// `all_encodings_list`, one owner held until `rshutdown`.
    pub(crate) all_encodings: Option<Owner<PhpArray>>,
    /// `current_detect_order_list`'s room: one `mbfl_encoding` pointer
    /// to each encoding of the detect order, `safe_emalloc`ed by `RINIT`
    /// and again by each `mb_detect_order()` that sets it, which the
    /// engine keeps as [`Settings::detect_order`].
    pub(crate) detect_order_room: Option<Emalloc>,
    pub(crate) regex: crate::regex::State,
}

impl State {
    pub(crate) fn identified_as(&mut self, source: Source) -> &mut Option<Encoding> {
        &mut self.identified_from[source as usize]
    }
}

impl Settings {
    /// `mb_internal_encoding($encoding)`: this request's, and set, so a
    /// change of php's own leaves it alone.
    pub(crate) fn set_current_internal(&mut self, encoding: Encoding) {
        self.current_internal = encoding;
        self.internal_origin = Origin::Own;
    }

    /// `mb_http_output($encoding)`.
    pub(crate) fn set_http_output(&mut self, encoding: Encoding) {
        self.http_output = encoding;
        self.http_output_origin = Origin::Own;
    }
}

/// The request's settings, built from the ini on first touch.
/// `php_mb_populate_current_detect_order_list`, and the swap
/// `mb_detect_order()` makes: the room for `entries` pointers of the
/// detect order, `safe_emalloc(entries, sizeof(mbfl_encoding *), 0)`,
/// laid down and the one before it given back after it. `RINIT`'s holds
/// the list's encodings; `mb_detect_order()`'s is what its parse
/// `ecalloc`s, a name or element for each and the language's default
/// list beside them for an `auto`, whatever the names turn out to be:
///
/// ```php
/// $m = memory_get_usage(); mb_detect_order('ASCII, UTF-8, SJIS, EUC-JP'); echo memory_get_usage() - $m, " ";
/// $m = memory_get_usage(); mb_detect_order(['ASCII', 'UTF-8']); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 32 -16
/// ```
///
/// The `Err` is the memory limit's fatal.
pub(crate) fn lay_detect_order_room(req: &mut Request, entries: usize) -> Result<(), Thrown> {
    let room = Emalloc::new(req, entries * std::mem::size_of::<usize>())?;
    if let Some(old) = req.state::<State>().detect_order_room.replace(room) {
        old.free(req);
    }
    Ok(())
}

pub(crate) fn settings(req: &mut Request) -> &mut Settings {
    if req.state::<State>().settings.is_none() {
        let built = Settings::from_ini(req);
        req.state::<State>().settings = Some(built);
    }
    req.state::<State>()
        .settings
        .get_or_insert_with(Settings::ginit)
}

impl Settings {
    /// `PHP_GINIT_FUNCTION(mbstring)`'s values, before any entry is read.
    fn ginit() -> Settings {
        Settings {
            language: Language::neutral(),
            default_detect: DEFAULT_DETECT[DEFAULT_DETECT.len() - 1].1,
            detect_order: Vec::new(),
            http_input: Vec::new(),
            internal: Encoding::utf8(),
            current_internal: Encoding::utf8(),
            http_output: Encoding::pass(),
            substitute: Substitute::DEFAULT,
            strict_detection: false,
            encoding_translation: false,
            internal_origin: Origin::Php,
            http_output_origin: Origin::Php,
            http_input_origin: Origin::Php,
        }
    }
}

/// `php_mb_default_identify_list`, by language, the neutral list last.
const DEFAULT_DETECT: [(i32, &[i32]); 9] = [
    (
        LANGUAGE_JAPANESE,
        &[
            ENCODING_ASCII,
            ENCODING_JIS,
            ENCODING_UTF8,
            ENCODING_EUC_JP,
            ENCODING_SJIS,
        ],
    ),
    (
        LANGUAGE_KOREAN,
        &[ENCODING_ASCII, ENCODING_UTF8, ENCODING_EUC_KR, ENCODING_UHC],
    ),
    (
        LANGUAGE_TRADITIONAL_CHINESE,
        &[
            ENCODING_ASCII,
            ENCODING_UTF8,
            ENCODING_EUC_TW,
            ENCODING_BIG5,
        ],
    ),
    (
        LANGUAGE_SIMPLIFIED_CHINESE,
        &[
            ENCODING_ASCII,
            ENCODING_UTF8,
            ENCODING_EUC_CN,
            ENCODING_CP936,
        ],
    ),
    (
        LANGUAGE_RUSSIAN,
        &[
            ENCODING_ASCII,
            ENCODING_UTF8,
            ENCODING_KOI8R,
            ENCODING_CP1251,
            ENCODING_CP866,
        ],
    ),
    (
        LANGUAGE_ARMENIAN,
        &[ENCODING_ASCII, ENCODING_UTF8, ENCODING_ARMSCII8],
    ),
    (
        LANGUAGE_TURKISH,
        &[
            ENCODING_ASCII,
            ENCODING_UTF8,
            ENCODING_CP1254,
            ENCODING_8859_9,
        ],
    ),
    (
        LANGUAGE_UKRAINIAN,
        &[ENCODING_ASCII, ENCODING_UTF8, ENCODING_KOI8U],
    ),
    (LANGUAGE_NEUTRAL, &[ENCODING_ASCII, ENCODING_UTF8]),
];

/// `php_mb_nls_get_default_detect_order_list`.
fn default_detect(language: Language) -> &'static [i32] {
    let neutral = DEFAULT_DETECT[DEFAULT_DETECT.len() - 1].1;
    DEFAULT_DETECT
        .iter()
        .find(|(no, _)| *no == language.no())
        .map_or(neutral, |(_, list)| list)
}

/// The encodings a list of numbers names.
fn numbered(list: &[i32]) -> Vec<Encoding> {
    list.iter()
        .filter_map(|&no| Encoding::numbered(no))
        .collect()
}

/// An entry's value, empty where none is set.
fn ini<'a>(req: &'a Request, name: &str) -> &'a [u8] {
    req.ini().get(name.as_bytes()).unwrap_or_default()
}

/// An encoding's name from mbstring's own entry where it is set, else
/// php's, and where it came from.
fn own_or_php(req: &Request, own: Vec<u8>, php: &str) -> (Origin, Vec<u8>) {
    match Origin::of(&own) {
        Origin::Own => (Origin::Own, own),
        Origin::Php => (Origin::Php, php_encoding(req, php, None)),
    }
}

/// `php_get_internal_encoding`, `php_get_input_encoding` and
/// `php_get_output_encoding` (`main/main.c`): php's own entry, then
/// `default_charset`, then `UTF-8`, `changed` standing in for the entry
/// a handler is about to write.
fn php_encoding(req: &Request, own: &str, changed: Option<(&str, &[u8])>) -> Vec<u8> {
    let read = |name: &str| match changed {
        Some((changing, value)) if changing == name => value.to_vec(),
        _ => ini(req, name).to_vec(),
    };
    [read(own), read("default_charset")]
        .into_iter()
        .find(|value| !value.is_empty())
        .unwrap_or_else(|| b"UTF-8".to_vec())
}

impl Settings {
    /// What `MINIT`'s entries and `mbstring_internal_encoding_changed_hook`
    /// leave, then `RINIT`'s copy, read from the ini with no diagnostic:
    /// each handler raised its own as the value was set.
    fn from_ini(req: &mut Request) -> Settings {
        let language =
            Language::named(ini(req, "mbstring.language")).unwrap_or_else(Language::neutral);
        let default_detect = default_detect(language);
        let detect_order = parse_list(ini(req, "mbstring.detect_order"), default_detect)
            .ok()
            .filter(|list| !list.is_empty())
            .unwrap_or_else(|| numbered(default_detect));
        let (http_input_origin, input) = own_or_php(
            req,
            ini(req, "mbstring.http_input").to_vec(),
            "input_encoding",
        );
        let http_input = http_input_list(&input, default_detect).unwrap_or_default();
        let (http_output_origin, output) = own_or_php(
            req,
            ini(req, "mbstring.http_output").to_vec(),
            "output_encoding",
        );
        let http_output = named_or_pass(&output).unwrap_or_else(Encoding::pass);
        let (internal_origin, internal) = own_or_php(
            req,
            ini(req, "mbstring.internal_encoding").to_vec(),
            "internal_encoding",
        );
        let internal = Encoding::named(&internal).unwrap_or_else(Encoding::utf8);
        let substitute = match req.ini().get(b"mbstring.substitute_character") {
            Some(value) if !value.is_empty() => Substitute::DEFAULT.updated(value),
            _ => Substitute::DEFAULT,
        };
        Settings {
            language,
            default_detect,
            detect_order,
            http_input,
            internal,
            current_internal: internal,
            http_output,
            substitute,
            strict_detection: Ini::parse_bool(ini(req, "mbstring.strict_detection")),
            encoding_translation: Ini::parse_bool(ini(req, "mbstring.encoding_translation")),
            internal_origin,
            http_output_origin,
            http_input_origin,
        }
    }
}

/// `php_mb_get_encoding_or_pass`: `pass`, or an encoding by name. php
/// compares with `strncmp(name, "pass", len)`, so any prefix of `pass`
/// is it, the empty name too.
pub(crate) fn named_or_pass(name: &[u8]) -> Option<Encoding> {
    if b"pass".starts_with(name) {
        return Some(Encoding::pass());
    }
    Encoding::named_ex(name)
}

/// Why a list of encodings was refused: the name that is none.
pub(crate) struct Invalid(pub(crate) Vec<u8>);

/// `php_mb_parse_encoding_list`: comma-separated names, quoted as a
/// whole or not, each trimmed of spaces and tabs, `auto` the language's
/// default list the first time it appears.
pub(crate) fn parse_list(value: &[u8], default_detect: &[i32]) -> Result<Vec<Encoding>, Invalid> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let value = if value.len() > 2 && value[0] == b'"' && value[value.len() - 1] == b'"' {
        &value[1..value.len() - 1]
    } else {
        value
    };
    let size = 1 + value.split(|&b| b == b',').count() - 1 + default_detect.len();
    let mut list = Vec::new();
    let mut auto = false;
    for word in value.split(|&b| b == b',') {
        let start = word
            .iter()
            .position(|b| !b" \t".contains(b))
            .unwrap_or(word.len());
        let end = word
            .iter()
            .rposition(|b| !b" \t".contains(b))
            .map_or(start, |e| e + 1);
        let word = &word[start..end];
        // `strncasecmp(word, "auto", len)`: `auto`, any prefix of it and
        // the empty word.
        if b"auto"
            .get(..word.len())
            .is_some_and(|auto| auto.eq_ignore_ascii_case(word))
        {
            if !auto {
                auto = true;
                list.extend(numbered(default_detect));
            }
        } else {
            list.push(Encoding::named_ex(word).ok_or_else(|| Invalid(word.to_vec()))?);
        }
        if list.len() >= size {
            break;
        }
    }
    Ok(list)
}

/// `_php_mb_ini_mbstring_http_input_set`'s parse: `pass` alone, or a list.
fn http_input_list(value: &[u8], default_detect: &[i32]) -> Result<Vec<Encoding>, Invalid> {
    if value == b"pass" {
        return Ok(vec![Encoding::pass()]);
    }
    parse_list(value, default_detect)
}

/// `php_mb_parse_encoding_list` with its diagnostics: a warning in ini
/// handler (`arg` none), a `ValueError` naming the argument otherwise.
pub(crate) fn encoding_list(
    req: &mut Request,
    value: &[u8],
    arg: Option<u32>,
) -> Result<Option<Vec<Encoding>>, Thrown> {
    let default_detect = settings(req).default_detect;
    match parse_list(value, default_detect) {
        Ok(list) => Ok(Some(list)),
        Err(Invalid(name)) => {
            // `%.*s`, which stops at a NUL.
            let name = String::from_utf8_lossy(php_ext::coerce::c_str(&name));
            if let Some(arg) = arg {
                Err(value_error(
                    req,
                    arg,
                    &format!("contains invalid encoding \"{name}\""),
                ))
            } else {
                docref(
                    req,
                    Level::Warning,
                    &format!("INI setting contains invalid encoding \"{name}\""),
                );
                Ok(None)
            }
        }
    }
}

/// `php_mb_parse_encoding_array`: each element as a string, `auto` the
/// default list once, an unknown name a `ValueError`.
pub(crate) fn encoding_array(
    req: &mut Request,
    array: PhpArray,
    arg: u32,
) -> Result<Vec<Encoding>, Thrown> {
    let default_detect = settings(req).default_detect;
    let mut list = Vec::new();
    let mut auto = false;
    // `ZEND_HASH_FOREACH_VAL(target_hash, hash_entry)`
    // (`Z/ext/mbstring/mbstring.c:385`), no owner of the table taken: the
    // argument holds it while `zval_try_get_string` runs `__toString`.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let value = row.value().deref();
        let invalid = php_ext::coerce::with_string(req, &value, |name| {
            if name.eq_ignore_ascii_case(b"auto") {
                if !auto {
                    auto = true;
                    list.extend(numbered(default_detect));
                }
                None
            } else if let Some(encoding) = Encoding::named(name) {
                list.push(encoding);
                None
            } else {
                Some(String::from_utf8_lossy(php_ext::coerce::c_str(name)).into_owned())
            }
        })?;
        if let Some(name) = invalid {
            return Err(value_error(
                req,
                arg,
                &format!("contains invalid encoding \"{name}\""),
            ));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(list)
}

/// `array|string` naming encodings, as the natives take it.
pub(crate) fn encodings(
    req: &mut Request,
    given: php_ext::abi::Union<PhpArray, PhpString>,
    arg: u32,
) -> Result<Vec<Encoding>, Thrown> {
    match given {
        php_ext::abi::Union::A(array) => encoding_array(req, array, arg),
        php_ext::abi::Union::B(names) => {
            Ok(encoding_list(req, names.as_bytes(), Some(arg))?.unwrap_or_default())
        }
    }
}

/// `php_mb_get_encoding`: the internal encoding for `null`, an encoding
/// by name otherwise, the last name looked up remembered, so a byte
/// encoding's deprecation is raised once for a series of calls naming it.
pub(crate) fn encoding(
    req: &mut Request,
    name: Option<PhpString>,
    arg: u32,
) -> Result<Encoding, Thrown> {
    let Some(name) = name else {
        return Ok(settings(req).current_internal);
    };
    let name = name.as_bytes();
    if let Some((last, encoding)) = &req.state::<State>().last_used
        && last.eq_ignore_ascii_case(name)
    {
        return Ok(*encoding);
    }
    let Some(encoding) = Encoding::named(name) else {
        let name = String::from_utf8_lossy(php_ext::coerce::c_str(name));
        return Err(value_error(
            req,
            arg,
            &format!("must be a valid encoding, \"{name}\" given"),
        ));
    };
    let deprecated = match encoding {
        e if e == Encoding::base64() => {
            Some("Base64 via mbstring is deprecated; use base64_encode/base64_decode")
        }
        e if e == Encoding::qprint() => Some(
            "QPrint via mbstring is deprecated; use quoted_printable_encode/quoted_printable_decode",
        ),
        e if e == Encoding::html_ent() => Some(
            "HTML entities via mbstring is deprecated; use htmlspecialchars, htmlentities, or mb_encode_numericentity/mb_decode_numericentity",
        ),
        e if e == Encoding::uuencode() => {
            Some("Uuencode via mbstring is deprecated; use convert_uuencode/convert_uudecode")
        }
        _ => None,
    };
    if let Some(text) = deprecated {
        docref(req, Level::Deprecated, &format!("Handling {text} instead"));
    }
    req.state::<State>().last_used = Some((name.to_vec(), encoding));
    Ok(encoding)
}

/// `remove_non_encodings_from_elist`: `Base64`, `QPrint` and their kin are
/// no text encodings and are never detected.
pub(crate) fn text_encodings(list: &mut Vec<Encoding>) {
    list.retain(|e| e.no() > ENCODING_CHARSET_MIN);
}

/// The ini entries, `PHP_INI_BEGIN()`'s order, a default `None` where
/// php registers the entry with no value.
pub(crate) fn entries() -> Vec<IniDecl> {
    let entry = |name: &'static str, default: Option<&'static str>, user: bool| IniDecl {
        name: Name(Bytes::borrowed(name.as_bytes())),
        default: default.map(|default| Bytes::borrowed(default.as_bytes())),
        user,
        per_dir: true,
        system: true,
    };
    vec![
        entry("mbstring.language", Some("neutral"), true),
        entry("mbstring.detect_order", None, true),
        entry("mbstring.http_input", None, true),
        entry("mbstring.http_output", None, true),
        entry("mbstring.internal_encoding", None, true),
        entry("mbstring.substitute_character", None, true),
        entry("mbstring.encoding_translation", Some("0"), false),
        entry(
            "mbstring.http_output_conv_mimetypes",
            Some("^(text/|application/xhtml\\+xml)"),
            true,
        ),
        entry("mbstring.strict_detection", Some("0"), true),
        entry("mbstring.regex_stack_limit", Some("100000"), true),
        entry("mbstring.regex_retry_limit", Some("1000000"), true),
    ]
}

/// The two entries `PHP_INI_BEGIN()` declares with `OnUpdateLong`.
pub(crate) const LONGS: [&[u8]; 2] = [b"mbstring.regex_stack_limit", b"mbstring.regex_retry_limit"];

/// The two `STD_PHP_INI_BOOLEAN` entries.
pub(crate) fn is_boolean(name: &[u8]) -> bool {
    name == b"mbstring.encoding_translation" || name == b"mbstring.strict_detection"
}

/// Each entry's handler, and the one php's `main.c` calls through
/// `php_internal_encoding_changed` for its own four.
pub(crate) fn handlers() -> [(&'static [u8], php_ext::abi::OnModify); 12] {
    [
        (b"mbstring.language", on_language),
        (b"mbstring.detect_order", on_detect_order),
        (b"mbstring.http_input", on_http_input),
        (b"mbstring.http_output", on_http_output),
        (b"mbstring.internal_encoding", on_internal_encoding),
        (b"mbstring.substitute_character", on_substitute_character),
        (b"mbstring.http_output_conv_mimetypes", on_conv_mimetypes),
        (b"mbstring.strict_detection", on_strict_detection),
        (b"default_charset", |req, value| {
            changed(req, "default_charset", value)
        }),
        (b"internal_encoding", |req, value| {
            changed(req, "internal_encoding", value)
        }),
        (b"input_encoding", |req, value| {
            changed(req, "input_encoding", value)
        }),
        (b"output_encoding", |req, value| {
            changed(req, "output_encoding", value)
        }),
    ]
}

/// `OnUpdate_mbstring_language`.
#[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature
fn on_language(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    let settings = settings(req);
    let Some(language) = Language::named(value) else {
        settings.language = Language::neutral();
        return Ok(false);
    };
    settings.language = language;
    settings.default_detect = default_detect(language);
    Ok(true)
}

/// `OnUpdate_mbstring_detect_order`: the list the next request starts
/// from, so this one's is left as it is.
fn on_detect_order(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    // No value, php's `NULL`, is no list, and taken.
    let Some(value) = value else {
        return Ok(true);
    };
    Ok(encoding_list(req, value, None)?.is_some_and(|list| !list.is_empty()))
}

/// `if (new_value) php_error_docref(...)`, the deprecation of
/// `mbstring.http_input`, `http_output` and `internal_encoding`, which
/// php registers with no value (`NULL`): raised for every value handed
/// in, the empty string among them, and for none where the entry is
/// handed back that `NULL`, as it is at the request's end where the
/// host did not set it. Where the host set it empty, the end hands that
/// back and deprecates it:
///
/// ```php
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'mbstring.http_input=', '-r', 'ini_set("mbstring.http_input", "UTF-8"); echo "end\n";'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]); proc_close($p);
/// ```
/// ```expect process
/// Deprecated: PHP Startup: Use of mbstring.http_input is deprecated in Unknown on line 0
///
/// Deprecated: ini_set(): Use of mbstring.http_input is deprecated in Command line code on line 1
/// end
///
/// Deprecated: PHP Request Shutdown: Use of mbstring.http_input is deprecated in Unknown on line 0
/// ```
///
/// Where it did not, the end hands back `NULL` and raises nothing:
///
/// ```php
/// ini_set('mbstring.http_output', 'UTF-8'); echo "end\n";
/// ```
/// ```expect
/// Deprecated: ini_set(): Use of mbstring.http_output is deprecated in %s on line 2
/// end
/// ```
fn deprecated_entry(req: &mut Request, name: &str, value: Option<&[u8]>) {
    if value.is_none() {
        return;
    }
    docref(
        req,
        Level::Deprecated,
        &format!("Use of mbstring.{name} is deprecated"),
    );
}

/// `OnUpdate_mbstring_http_input`.
fn on_http_input(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    deprecated_entry(req, "http_input", value);
    let own = value.unwrap_or_default().to_vec();
    let (origin, value) = own_or_php(req, own, "input_encoding");
    settings(req).http_input_origin = origin;
    set_http_input(req, &value).map(|done| done || origin == Origin::Php)
}

/// `_php_mb_ini_mbstring_http_input_set`.
fn set_http_input(req: &mut Request, value: &[u8]) -> Result<bool, Thrown> {
    let list = if value == b"pass" {
        Some(vec![Encoding::pass()])
    } else {
        encoding_list(req, value, None)?
    };
    match list {
        Some(list) if !list.is_empty() => {
            settings(req).http_input = list;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// `OnUpdate_mbstring_http_output`.
#[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature
fn on_http_output(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    deprecated_entry(req, "http_output", value);
    let own = value.unwrap_or_default().to_vec();
    let (origin, value) = own_or_php(req, own, "output_encoding");
    let settings = settings(req);
    settings.http_output_origin = origin;
    match named_or_pass(&value) {
        Some(encoding) => {
            settings.http_output = encoding;
            Ok(true)
        }
        None => Ok(origin == Origin::Php),
    }
}

/// `OnUpdate_mbstring_internal_encoding`.
#[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature
fn on_internal_encoding(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    deprecated_entry(req, "internal_encoding", value);
    let own = value.unwrap_or_default().to_vec();
    let (origin, value) = own_or_php(req, own, "internal_encoding");
    settings(req).internal_origin = origin;
    set_internal(req, &value);
    Ok(true)
}

/// `_php_mb_ini_mbstring_internal_encoding_set`: an unknown name is
/// UTF-8, after a warning; the regex encoding follows it.
fn set_internal(req: &mut Request, value: &[u8]) {
    let encoding = Encoding::named(value).unwrap_or_else(|| {
        let name = String::from_utf8_lossy(php_ext::coerce::c_str(value));
        docref(
            req,
            Level::Warning,
            &format!("Unknown encoding \"{name}\" in ini setting"),
        );
        Encoding::utf8()
    });
    let settings = settings(req);
    settings.internal = encoding;
    settings.current_internal = encoding;
    crate::regex::internal_encoding_changed(req, value);
}

/// `OnUpdate_mbstring_substitute_character`: no value, php's `NULL`,
/// which `ini_restore()` hands back, is the character `?`, and the
/// detect order's `NULL` is no list, taken:
///
/// ```php
/// ini_set('mbstring.substitute_character', '65'); ini_restore('mbstring.substitute_character');
/// ini_set('mbstring.detect_order', 'ASCII'); ini_restore('mbstring.detect_order');
/// var_dump(mb_substitute_character(), ini_get('mbstring.detect_order'));
/// ```
/// ```expect
/// int(63)
/// string(0) ""
/// ```
#[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature
fn on_substitute_character(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let settings = settings(req);
    settings.substitute = match value {
        Some(value) => settings.substitute.updated(value),
        // No value, php's `NULL`: the character `?`.
        None => Substitute {
            mode: Mode::Char,
            char: 0x3f,
        },
    };
    Ok(true)
}

/// `OnUpdate_mbstring_http_output_conv_mimetypes`: the pattern compiled
/// by pcre, a pattern that does not compile refused.
#[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature
fn on_conv_mimetypes(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    Ok(crate::http::conv_mimetypes(req, value).is_ok())
}

/// `OnUpdateBool` over `strict_detection`.
#[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature
fn on_strict_detection(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    settings(req).strict_detection = Ini::parse_bool(value);
    Ok(true)
}

/// `mbstring_internal_encoding_changed_hook`, served before php's own
/// entry `name` takes `value`: each of the three encodings its entry did
/// not set follows php's. At startup php has called it once already, as
/// `MINIT` ends, after `REGISTER_INI_ENTRIES` handed each entry the host
/// left unset its default, so what an unknown encoding raises there is
/// raised twice; this raises the registration's before the hook's.
fn changed(req: &mut Request, name: &str, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    let (internal, http_output, http_input) = {
        let s = settings(req);
        (s.internal_origin, s.http_output_origin, s.http_input_origin)
    };
    let change = Some((name, value));
    if req.ini().stage() == IniStage::Startup {
        if http_input == Origin::Php {
            let encoding = php_encoding(req, "input_encoding", change);
            set_http_input(req, &encoding)?;
        }
        if internal == Origin::Php {
            let encoding = php_encoding(req, "internal_encoding", change);
            set_internal(req, &encoding);
        }
    }
    if internal == Origin::Php {
        let encoding = php_encoding(req, "internal_encoding", change);
        set_internal(req, &encoding);
    }
    if http_output == Origin::Php {
        let encoding = php_encoding(req, "output_encoding", change);
        if let Some(encoding) = named_or_pass(&encoding) {
            settings(req).http_output = encoding;
        }
    }
    if http_input == Origin::Php {
        let encoding = php_encoding(req, "input_encoding", change);
        set_http_input(req, &encoding)?;
    }
    Ok(true)
}
