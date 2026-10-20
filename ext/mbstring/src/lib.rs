//! The `mbstring` extension: `php-src/ext/mbstring` as one crate under
//! `ext/` (ADR-0004), over php-src's libmbfl, vendored under its LGPL
//! and compiled by `build.rs`, and the platform's oniguruma (design
//! §7.2, revision 47, #1173). One dependency beside the facade for each
//! extension php's mbstring includes: ext/pcre for
//! `mbstring.http_output_conv_mimetypes`, ext/standard for `php_mail`.
//! One entry, `extension()`; its state on the value (ADR-0005). The
//! phpt subset is `php-src/ext/mbstring/tests`, through `xtask phpt
//! --subset`.
//!
//! libmbfl is php's for every conversion: the 79 encodings, their error
//! markers and the kana table. What is ported here is php's own C, which
//! drives it: `mbstring.c`'s natives and ini handling ([`settings`],
//! [`text`], [`convert`], [`mime`], [`http`]), `php_unicode.c`'s case
//! mapping ([`case`]), `mb_gpc.c`'s query parsing ([`http`]) and
//! `php_mbregex.c` ([`regex`]).
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

mod case;
mod convert;
mod ffi;
mod http;
mod mime;
mod regex;
mod settings;
mod tables;
mod text;
mod zend;

use ffi::mbfl::{Encoding, Language};
use php_ext::abi::{
    Dependency, DependencyKind, Extension, ExtensionBuilder, LongHandler, OrFalse, Path, Union,
};
use php_ext::attr::{php_const, php_function};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{InfoRow, Ini, Request};
use php_ext::values::{Owner, PhpArray, PhpString};
use settings::{Mode, State, Substitute, named_or_pass, settings};
use tables::{MBFL_VERSION_MAJOR, MBFL_VERSION_MINOR, MBFL_VERSION_TEENY};
use text::{new_string, value_error};

/// `mbstring.stub.php`'s constants.
#[php_const]
mod constants {
    pub(crate) const MB_ONIGURUMA_VERSION: &str = env!("ONIG_VERSION");
    pub(crate) const MB_CASE_UPPER: i64 = 0;
    pub(crate) const MB_CASE_LOWER: i64 = 1;
    pub(crate) const MB_CASE_TITLE: i64 = 2;
    pub(crate) const MB_CASE_FOLD: i64 = 3;
    pub(crate) const MB_CASE_UPPER_SIMPLE: i64 = 4;
    pub(crate) const MB_CASE_LOWER_SIMPLE: i64 = 5;
    pub(crate) const MB_CASE_TITLE_SIMPLE: i64 = 6;
    pub(crate) const MB_CASE_FOLD_SIMPLE: i64 = 7;
}

/// The extension value (design §7.1): the natives in the stub's order,
/// the constants, the ini entries with their handlers, and the state
/// declared on it.
///
/// ```php
/// var_dump(extension_loaded('mbstring'), MB_CASE_TITLE, MB_CASE_FOLD_SIMPLE, ini_get('mbstring.language'), ini_get('mbstring.regex_stack_limit'));
/// ```
/// ```expect
/// bool(true)
/// int(2)
/// int(7)
/// string(7) "neutral"
/// string(6) "100000"
/// ```
pub fn extension() -> Extension {
    let mut builder = ExtensionBuilder::default()
        .functions(vec![
            mb_language::declared(),
            mb_internal_encoding::declared(),
            http::mb_http_input::declared(),
            mb_http_output::declared(),
            mb_detect_order::declared(),
            mb_substitute_character::declared(),
            mb_preferred_mime_name::declared(),
            http::mb_parse_str::declared(),
            http::mb_output_handler::declared(),
            text::mb_str_split::declared(),
            text::mb_strlen::declared(),
            text::mb_strpos::declared(),
            text::mb_strrpos::declared(),
            text::mb_stripos::declared(),
            text::mb_strripos::declared(),
            text::mb_strstr::declared(),
            text::mb_strrchr::declared(),
            text::mb_stristr::declared(),
            text::mb_strrichr::declared(),
            text::mb_substr_count::declared(),
            text::mb_substr::declared(),
            text::mb_strcut::declared(),
            text::mb_strwidth::declared(),
            text::mb_strimwidth::declared(),
            convert::mb_convert_encoding::declared(),
            case::mb_convert_case::declared(),
            case::mb_strtoupper::declared(),
            case::mb_strtolower::declared(),
            text::mb_ucfirst::declared(),
            text::mb_lcfirst::declared(),
            text::mb_trim::declared(),
            text::mb_ltrim::declared(),
            text::mb_rtrim::declared(),
            convert::mb_detect_encoding::declared(),
            convert::mb_list_encodings::declared(),
            convert::mb_encoding_aliases::declared(),
            mime::mb_encode_mimeheader::declared(),
            mime::mb_decode_mimeheader::declared(),
            convert::mb_convert_kana::declared(),
            convert::mb_convert_variables::declared(),
            convert::mb_encode_numericentity::declared(),
            convert::mb_decode_numericentity::declared(),
            mime::mb_send_mail::declared(),
            mb_get_info::declared(),
            convert::mb_check_encoding::declared(),
            text::mb_scrub::declared(),
            text::mb_ord::declared(),
            text::mb_chr::declared(),
            text::mb_str_pad::declared(),
            regex::mb_regex_encoding::declared(),
            regex::mb_ereg::declared(),
            regex::mb_eregi::declared(),
            regex::mb_ereg_replace::declared(),
            regex::mb_eregi_replace::declared(),
            regex::mb_ereg_replace_callback::declared(),
            regex::mb_split::declared(),
            regex::mb_ereg_match::declared(),
            regex::mb_ereg_search::declared(),
            regex::mb_ereg_search_pos::declared(),
            regex::mb_ereg_search_regs::declared(),
            regex::mb_ereg_search_init::declared(),
            regex::mb_ereg_search_getregs::declared(),
            regex::mb_ereg_search_getpos::declared(),
            regex::mb_ereg_search_setpos::declared(),
            regex::mb_regex_set_options::declared(),
        ])
        .constants(constants::declared())
        .ini(settings::entries())
        .dependencies(vec![Dependency {
            name: "pcre".into(),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .info(info)
        .treat_data(http::treat_data)
        .encodings(zend::bind)
        .process_state(|| Ok(regex::initialize()))
        .request_state::<State>()
        .rinit(rinit)
        .rshutdown(rshutdown);
    for (name, handler) in settings::handlers() {
        builder = builder.on_modify(name, handler);
    }
    for name in settings::LONGS {
        builder = builder.on_update_long(name, LongHandler::Any);
    }
    builder.build("mbstring")
}

/// `PHP_RINIT_FUNCTION(mbstring)`'s `php_mb_populate_current_detect_order_list`:
/// the room for the detect order's pointers, 16 bytes for the neutral
/// language's two, which `memory_get_usage()` counts from the first line.
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    let entries = settings(req).detect_order.len();
    settings::lay_detect_order_room(req, entries)
}

/// `PHP_RSHUTDOWN_FUNCTION(mbstring)`: the detect order's room freed, the
/// owner of `mb_list_encodings()`'s array given back, and the regex
/// state's.
fn rshutdown(req: &mut Request) {
    if let Some(room) = req.state::<State>().detect_order_room.take() {
        room.free(req);
    }
    if let Some(all) = req.state::<State>().all_encodings.take() {
        all.give_up(req);
    }
    regex::rshutdown(req);
}

/// `PHP_MINFO_FUNCTION(mbstring)`, with `mb_regex`'s table, then
/// `DISPLAY_INI_ENTRIES`.
///
/// ```php
/// $e = new ReflectionExtension('mbstring'); ob_start(); $e->info(); echo ob_get_clean();
/// ```
/// ```expect
///
/// mbstring
///
/// Multibyte Support => enabled
/// Multibyte string engine => libmbfl
/// HTTP input encoding translation => disabled
/// libmbfl version => 1.3.2
///
/// mbstring extension makes use of "streamable kanji code filter and converter", which is distributed under the GNU Lesser General Public License version 2.1.
///
/// Multibyte (japanese) regex support => enabled
/// Multibyte regex (oniguruma) version => 6.9.10
///
/// Directive => Local Value => Master Value
/// mbstring.language => neutral => neutral
/// mbstring.detect_order => no value => no value
/// mbstring.http_input => no value => no value
/// mbstring.http_output => no value => no value
/// mbstring.internal_encoding => no value => no value
/// mbstring.substitute_character => no value => no value
/// mbstring.encoding_translation => Off => Off
/// mbstring.http_output_conv_mimetypes => ^(text/|application/xhtml\+xml) => ^(text/|application/xhtml\+xml)
/// mbstring.strict_detection => Off => Off
/// mbstring.regex_stack_limit => 100000 => 100000
/// mbstring.regex_retry_limit => 1000000 => 1000000
/// ```
fn info(req: &mut Request) {
    let translation = if settings(req).encoding_translation {
        "enabled"
    } else {
        "disabled"
    };
    let version = format!("{MBFL_VERSION_MAJOR}.{MBFL_VERSION_MINOR}.{MBFL_VERSION_TEENY}");
    req.extensions_mut().info_table(&[
        &["Multibyte Support", "enabled"],
        &["Multibyte string engine", "libmbfl"],
        &["HTTP input encoding translation", translation],
        &["libmbfl version", &version],
    ]);
    req.extensions_mut().info_rows(&[InfoRow::Header(&[
        "mbstring extension makes use of \"streamable kanji code filter and converter\", which is distributed under the GNU Lesser General Public License version 2.1.",
    ])]);
    regex::info(req);
    display_ini_entries(req);
}

/// `DISPLAY_INI_ENTRIES()`: each entry's local and master value, a
/// `STD_PHP_INI_BOOLEAN` one through `zend_ini_boolean_displayer_cb` as
/// `On` or `Off`, in `display_ini_entries`' table, an empty one `no
/// value`. In HTML the table is php's directive table and the licence
/// line above it a header, `php_info_print_table_header(1, ..)`, as
/// `phpinfo()`'s page shows them (#2337):
///
/// ```php
/// ob_start(); (new ReflectionExtension('mbstring'))->info(); $page = ob_get_clean();
/// echo strstr($page, '<tr class="h"><th>mbstring');
/// ```
/// ```request
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.9
/// Content-type: text/html; charset=UTF-8
///
/// <tr class="h"><th>mbstring extension makes use of "streamable kanji code filter and converter", which is distributed under the GNU Lesser General Public License version 2.1.</th></tr>
/// </table>
/// <table>
/// <tr><td class="e">Multibyte (japanese) regex support </td><td class="v">enabled </td></tr>
/// <tr><td class="e">Multibyte regex (oniguruma) version </td><td class="v">6.9.10 </td></tr>
/// </table>
/// <table>
/// <tr class="h"><th>Directive</th><th>Local Value</th><th>Master Value</th></tr>
/// <tr><td class="e">mbstring.language</td><td class="v">neutral</td><td class="v">neutral</td></tr>
/// <tr><td class="e">mbstring.detect_order</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
/// <tr><td class="e">mbstring.http_input</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
/// <tr><td class="e">mbstring.http_output</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
/// <tr><td class="e">mbstring.internal_encoding</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
/// <tr><td class="e">mbstring.substitute_character</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
/// <tr><td class="e">mbstring.encoding_translation</td><td class="v">Off</td><td class="v">Off</td></tr>
/// <tr><td class="e">mbstring.http_output_conv_mimetypes</td><td class="v">^(text/|application/xhtml\+xml)</td><td class="v">^(text/|application/xhtml\+xml)</td></tr>
/// <tr><td class="e">mbstring.strict_detection</td><td class="v">Off</td><td class="v">Off</td></tr>
/// <tr><td class="e">mbstring.regex_stack_limit</td><td class="v">100000</td><td class="v">100000</td></tr>
/// <tr><td class="e">mbstring.regex_retry_limit</td><td class="v">1000000</td><td class="v">1000000</td></tr>
/// </table>
/// ```
fn display_ini_entries(req: &mut Request) {
    let Some(me) = req.extensions().named(b"mbstring") else {
        return;
    };
    let shown = |name: &[u8], value: &[u8]| -> String {
        if settings::is_boolean(name) {
            return (if Ini::parse_bool(value) { "On" } else { "Off" }).to_owned();
        }
        String::from_utf8_lossy(value).into_owned()
    };
    let rows: Vec<[String; 3]> = req
        .ini()
        .extension_ini(me)
        .into_iter()
        .map(|held| {
            let name = &held.decl.name.0;
            [
                String::from_utf8_lossy(name).into_owned(),
                shown(name, held.value.as_deref().unwrap_or_default()),
                shown(name, held.master.as_deref().unwrap_or_default()),
            ]
        })
        .collect();
    let rows: Vec<[&str; 3]> = (rows.iter())
        .map(|[n, l, m]| [n.as_str(), l.as_str(), m.as_str()])
        .collect();
    req.ini_mut().info_ini_table(&rows);
}

/// `mb_language(?string $language = null): string|bool`.
///
/// ```php
/// var_dump(mb_language(), mb_language("ja"), mb_language(), mb_language("uni"), mb_language());
/// try { mb_language("nope"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(7) "neutral"
/// bool(true)
/// string(8) "Japanese"
/// bool(true)
/// string(3) "uni"
/// mb_language(): Argument #1 ($language) must be a valid language, "nope" given
/// ```
///
/// The entry keeps the string it is handed (`Z/ext/mbstring/mbstring.c:1216`):
///
/// ```php
/// ini_set('from', 'x');
/// $v = str_repeat('j', 2); $v[1] = 'a'; $m = memory_get_usage(); mb_language($v); unset($v); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 0
/// ```
#[php_function]
fn mb_language(
    req: &mut Request,
    #[php(default = null)] language: Option<PhpString>,
) -> Result<Union<Owner<PhpString>, bool>, Thrown> {
    let Some(name) = language else {
        let current = settings(req).language;
        return Ok(Union::A(new_string(req, current.name())?));
    };
    if req
        .ini_mut()
        .set_string(b"mbstring.language", name)?
        .is_none()
    {
        let name = String::from_utf8_lossy(name.as_bytes()).into_owned();
        return Err(value_error(
            req,
            1,
            &format!("must be a valid language, \"{name}\" given"),
        ));
    }
    Ok(Union::B(true))
}

/// `mb_internal_encoding(?string $encoding = null): string|bool`.
///
/// ```php
/// var_dump(mb_internal_encoding(), mb_internal_encoding("sjis"), mb_internal_encoding());
/// try { mb_internal_encoding("nope"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(5) "UTF-8"
/// bool(true)
/// string(4) "SJIS"
/// mb_internal_encoding(): Argument #1 ($encoding) must be a valid encoding, "nope" given
/// ```
#[php_function]
fn mb_internal_encoding(
    req: &mut Request,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Union<Owner<PhpString>, bool>, Thrown> {
    let Some(name) = encoding else {
        let current = settings(req).current_internal;
        return Ok(Union::A(new_string(req, current.name())?));
    };
    let Some(enc) = Encoding::named(name.as_bytes()) else {
        let name = String::from_utf8_lossy(name.as_bytes()).into_owned();
        return Err(value_error(
            req,
            1,
            &format!("must be a valid encoding, \"{name}\" given"),
        ));
    };
    settings(req).set_current_internal(enc);
    Ok(Union::B(true))
}

/// `mb_http_output(?string $encoding = null): string|bool`.
///
/// ```php
/// var_dump(mb_http_output(), mb_http_output("EUC-JP"), mb_http_output(), mb_http_output("pa"), mb_http_output());
/// try { mb_http_output("nope"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(5) "UTF-8"
/// bool(true)
/// string(6) "EUC-JP"
/// bool(true)
/// string(4) "pass"
/// mb_http_output(): Argument #1 ($encoding) must be a valid encoding, "nope" given
/// ```
#[php_function]
fn mb_http_output(
    req: &mut Request,
    #[php(default = null)] encoding: Option<Path>,
) -> Result<Union<Owner<PhpString>, bool>, Thrown> {
    let Some(Path(name)) = encoding else {
        let current = settings(req).http_output;
        return Ok(Union::A(new_string(req, current.name())?));
    };
    let Some(enc) = named_or_pass(name.as_bytes()) else {
        let name = String::from_utf8_lossy(name.as_bytes()).into_owned();
        return Err(value_error(
            req,
            1,
            &format!("must be a valid encoding, \"{name}\" given"),
        ));
    };
    settings(req).set_http_output(enc);
    Ok(Union::B(true))
}

/// The names of `list` as an array.
fn names(req: &mut Request, list: &[Encoding]) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for enc in list {
        let name = new_string(req, enc.name())?;
        out.append(req, name.into_value())?;
    }
    Ok(out)
}

/// `mb_detect_order(array|string|null $encoding = null): array|bool`.
///
/// ```php
/// var_dump(mb_detect_order(), mb_detect_order("ASCII, auto, SJIS"), mb_detect_order());
/// try { mb_detect_order([]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(5) "ASCII"
///   [1]=>
///   string(5) "UTF-8"
/// }
/// bool(true)
/// array(4) {
///   [0]=>
///   string(5) "ASCII"
///   [1]=>
///   string(5) "ASCII"
///   [2]=>
///   string(5) "UTF-8"
///   [3]=>
///   string(4) "SJIS"
/// }
/// mb_detect_order(): Argument #1 ($encoding) must specify at least one encoding
/// ```
#[php_function]
fn mb_detect_order(
    req: &mut Request,
    #[php(default = null)] encoding: Option<Union<PhpArray, PhpString>>,
) -> Result<Union<Owner<PhpArray>, bool>, Thrown> {
    let Some(given) = encoding else {
        let list = settings(req).detect_order.clone();
        return Ok(Union::A(names(req, &list)?));
    };
    // `php_mb_parse_encoding_array`'s and `php_mb_parse_encoding_list`'s
    // `ecalloc`: room for each element or name, and the language's
    // default list beside them for an `auto`.
    let given_entries = match &given {
        Union::A(array) => array.len(),
        Union::B(names) => names.as_bytes().split(|b| *b == b',').count(),
    };
    let list = settings::encodings(req, given, 1)?;
    if list.is_empty() {
        return Err(value_error(req, 1, "must specify at least one encoding"));
    }
    let entries = given_entries + settings(req).default_detect.len();
    settings(req).detect_order = list;
    settings::lay_detect_order_room(req, entries)?;
    Ok(Union::B(true))
}

/// `php_mb_check_code_point`: a codepoint, no surrogate.
fn is_code_point(cp: i64) -> bool {
    (0..0x11_0000).contains(&cp) && !(0xD800..=0xDFFF).contains(&cp)
}

/// `mb_substitute_character(string|int|null $substitute_character = null): string|int|bool`.
///
/// ```php
/// var_dump(mb_substitute_character(), mb_substitute_character("long"), mb_substitute_character(), mb_substitute_character(0x3013), mb_substitute_character());
/// try { mb_substitute_character("x"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mb_substitute_character(0xD800); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(63)
/// bool(true)
/// string(4) "long"
/// bool(true)
/// int(12307)
/// mb_substitute_character(): Argument #1 ($substitute_character) must be "none", "long", "entity" or a valid codepoint
/// mb_substitute_character(): Argument #1 ($substitute_character) is not a valid codepoint
/// ```
#[php_function]
fn mb_substitute_character(
    req: &mut Request,
    #[php(default = null)] substitute_character: Option<Union<PhpString, i64>>,
) -> Result<Union<Owner<PhpString>, Union<i64, bool>>, Thrown> {
    let current = settings(req).substitute;
    let next = match substitute_character {
        None => {
            return Ok(match current.shown(req)? {
                Union::A(name) => Union::A(name),
                Union::B(char) => Union::B(Union::A(char)),
            });
        }
        Some(Union::A(name)) => match Substitute::named(name.as_bytes()) {
            Some(mode) => Substitute { mode, ..current },
            None => {
                return Err(value_error(
                    req,
                    1,
                    "must be \"none\", \"long\", \"entity\" or a valid codepoint",
                ));
            }
        },
        Some(Union::B(cp)) if is_code_point(cp) => Substitute {
            mode: Mode::Char,
            char: u32::try_from(cp).unwrap_or(u32::from(b'?')),
        },
        Some(Union::B(_)) => return Err(value_error(req, 1, "is not a valid codepoint")),
    };
    settings(req).substitute = next;
    Ok(Union::B(Union::B(true)))
}

/// `mb_preferred_mime_name(string $encoding): string|false`.
///
/// ```php
/// var_dump(mb_preferred_mime_name("sjis-win"), mb_preferred_mime_name("UTF-8"), mb_preferred_mime_name("UTF7-IMAP"));
/// try { mb_preferred_mime_name("nope"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Warning: mb_preferred_mime_name(): No MIME preferred name corresponding to "UTF7-IMAP" in %s on line 2
/// string(9) "Shift_JIS"
/// string(5) "UTF-8"
/// bool(false)
/// mb_preferred_mime_name(): Argument #1 ($encoding) must be a valid encoding, "nope" given
/// ```
#[php_function]
fn mb_preferred_mime_name(
    req: &mut Request,
    encoding: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let text = String::from_utf8_lossy(encoding.as_bytes()).into_owned();
    let Some(enc) = Encoding::named(encoding.as_bytes()) else {
        return Err(value_error(
            req,
            1,
            &format!("must be a valid encoding, \"{text}\" given"),
        ));
    };
    if let Some(name) = enc.preferred_mime_name() {
        Ok(OrFalse::Value(new_string(req, name)?))
    } else {
        docref(
            req,
            Level::Warning,
            &format!("No MIME preferred name corresponding to \"{text}\""),
        );
        Ok(OrFalse::False)
    }
}

/// One of `mb_get_info()`'s answers: a list, a name or a number.
type Info = Union<Owner<PhpArray>, Union<Owner<PhpString>, i64>>;

/// `mb_get_info()`'s keys, in the order `"all"` lists them.
const INFO_KEYS: [&str; 13] = [
    "internal_encoding",
    "http_input",
    "http_output",
    "http_output_conv_mimetypes",
    "mail_charset",
    "mail_header_encoding",
    "mail_body_encoding",
    "illegal_chars",
    "encoding_translation",
    "language",
    "detect_order",
    "substitute_character",
    "strict_detection",
];

/// One entry of `mb_get_info()` by its key: `None` where php's `"all"`
/// leaves it out, `http_input` before any input was identified and an
/// empty `detect_order`.
fn info_entry(req: &mut Request, key: &str, language: Language) -> Result<Option<Info>, Thrown> {
    let s = settings(req);
    let (internal, output, detect, sub, strict, translation) = (
        s.current_internal,
        s.http_output,
        s.detect_order.clone(),
        s.substitute,
        s.strict_detection,
        s.encoding_translation,
    );
    let [mail_charset, mail_header, mail_body] = mime::mail_names(language);
    let text = |req: &mut Request, bytes: &[u8]| -> Result<Option<Info>, Thrown> {
        Ok(Some(Union::B(Union::A(new_string(req, bytes)?))))
    };
    let on_off = |req: &mut Request, on: bool| text(req, if on { b"On" } else { b"Off" });
    match key {
        "internal_encoding" => text(req, internal.name()),
        "http_input" => match req.state::<State>().identified {
            Some(enc) => text(req, enc.name()),
            None => Ok(None),
        },
        "http_output" => text(req, output.name()),
        "http_output_conv_mimetypes" => {
            let value = req
                .ini()
                .get(b"mbstring.http_output_conv_mimetypes")
                .unwrap_or_default()
                .to_vec();
            text(req, &value)
        }
        "mail_charset" => text(req, mail_charset),
        "mail_header_encoding" => text(req, mail_header),
        "mail_body_encoding" => text(req, mail_body),
        "illegal_chars" => {
            let count = req.state::<State>().illegal_chars;
            Ok(Some(Union::B(Union::B(
                i64::try_from(count).unwrap_or(i64::MAX),
            ))))
        }
        "encoding_translation" => on_off(req, translation),
        "language" => text(req, language.name()),
        "detect_order" if detect.is_empty() => Ok(None),
        "detect_order" => Ok(Some(Union::A(names(req, &detect)?))),
        "substitute_character" => Ok(Some(Union::B(sub.shown(req)?))),
        _ => on_off(req, strict),
    }
}

/// `mb_get_info(string $type = "all"): array|string|int|false|null`.
///
/// ```php
/// var_dump(mb_get_info("internal_encoding"), mb_get_info("http_input"), mb_get_info("language"), mb_get_info("mail_charset"), mb_get_info("illegal_chars"), mb_get_info("strict_detection"), mb_get_info("nope"));
/// var_dump(array_keys(mb_get_info()));
/// ```
/// ```expect
///
/// Warning: mb_get_info(): argument #1 ($type) must be a valid type in %s on line 2
/// string(5) "UTF-8"
/// NULL
/// string(7) "neutral"
/// string(5) "UTF-8"
/// int(0)
/// string(3) "Off"
/// bool(false)
/// array(12) {
///   [0]=>
///   string(17) "internal_encoding"
///   [1]=>
///   string(11) "http_output"
///   [2]=>
///   string(26) "http_output_conv_mimetypes"
///   [3]=>
///   string(12) "mail_charset"
///   [4]=>
///   string(20) "mail_header_encoding"
///   [5]=>
///   string(18) "mail_body_encoding"
///   [6]=>
///   string(13) "illegal_chars"
///   [7]=>
///   string(20) "encoding_translation"
///   [8]=>
///   string(8) "language"
///   [9]=>
///   string(12) "detect_order"
///   [10]=>
///   string(20) "substitute_character"
///   [11]=>
///   string(16) "strict_detection"
/// }
/// ```
#[php_function]
fn mb_get_info(
    req: &mut Request,
    #[php(default = "all")] r#type: PhpString,
) -> Result<Option<OrFalse<Info>>, Thrown> {
    let language = settings(req).language;
    let name = r#type.as_bytes().to_ascii_lowercase();
    if name == b"all" {
        let mut out = PhpArray::new(req)?;
        for key in INFO_KEYS {
            if let Some(value) = info_entry(req, key, language)? {
                let value = match value {
                    Union::A(list) => list.into_value(),
                    Union::B(Union::A(text)) => text.into_value(),
                    Union::B(Union::B(n)) => Owner::int(n),
                };
                out.set_str(req, key.as_bytes(), value)?;
            }
        }
        return Ok(Some(OrFalse::Value(Union::A(out))));
    }
    let Some(key) = INFO_KEYS.into_iter().find(|key| key.as_bytes() == name) else {
        docref(
            req,
            Level::Warning,
            "argument #1 ($type) must be a valid type",
        );
        return Ok(Some(OrFalse::False));
    };
    Ok(info_entry(req, key, language)?.map(OrFalse::Value))
}
