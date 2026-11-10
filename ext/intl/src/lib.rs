//! The `intl` extension: `php-src/ext/intl` as one crate under `ext/`
//! (ADR-0004), behind the `intl` feature as php builds it `--enable-intl`,
//! over the ICU4C the pinned keg links (`icu4c@<major>`, the major the
//! pin's `[closure]` names; #4406), found through `pkg-config` and bound
//! in [`ffi`], never vendored; php's intl C++ over ICU's C++ classes is
//! reached through the `extern "C"` shim in
//! `src/ffi/shim/` (design §7.2, revision 75; #1573). Its state on the
//! value (ADR-0005). The phpt subset is `php-src/ext/intl/tests`, through
//! `xtask phpt --subset`.
//!
//! `php_intl.c` and `common/common_error.c` are this file: php's
//! `intl_module_entry`, its ini entries and info rows, the constants
//! `register_php_intl_symbols`, `collator_register_Collator_symbols` and
//! `intl_register_common_symbols` lay down in that order, `IntlException`
//! and the `intl_*` error functions. [`error`] is `intl_error.c`. Each
//! area of php's tree is a Rust `mod` of its own, its functions laid in the
//! stub's block order and its classes in `PHP_MINIT_FUNCTION(intl)`'s.
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

// The error state and the binding's base are what every area calls; the
// areas land one at a time over them (#1574).
mod breakiterator;
mod calendar;
mod collator;
mod common_date;
mod converter;
mod dateformat;
mod datepatterngenerator;
#[allow(dead_code)]
mod error;
#[allow(dead_code)]
mod ffi;
mod formatter;
mod grapheme;
mod idn;
#[allow(dead_code)]
mod iterator;
mod listformatter;
mod locale;
mod msgformat;
mod normalizer;
mod resourcebundle;
mod spoofchecker;
mod throwables;
mod timezone;
mod transliterator;
mod uchar;
mod utf8;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::decl::IniDecl;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Ini, Request};
use php_ext::values::{Owner, PhpString};

use error::IntlError;
use ffi::Code;

/// `intl.default_locale`, `LOCALE_INI_NAME`.
pub(crate) const DEFAULT_LOCALE: &[u8] = b"intl.default_locale";
/// `intl.error_level`.
const ERROR_LEVEL: &[u8] = b"intl.error_level";
/// `intl.use_exceptions`.
const USE_EXCEPTIONS: &[u8] = b"intl.use_exceptions";

/// Every global constant in `PHP_MINIT_FUNCTION(intl)`'s order, with
/// `INTL_ICU_VERSION` and `INTL_ICU_DATA_VERSION` the linked ICU's.
///
/// ```php
/// var_dump(INTL_MAX_LOCALE_LEN, INTL_ICU_VERSION === INTL_ICU_DATA_VERSION, IDNA_DEFAULT, ULOC_VALID_LOCALE, U_USING_FALLBACK_WARNING, U_ERROR_LIMIT, count(get_defined_constants(true)['intl']));
/// ```
/// ```expect
/// int(156)
/// bool(true)
/// int(48)
/// int(1)
/// int(-128)
/// int(66818)
/// int(170)
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const INTL_MAX_LOCALE_LEN: i64 = 156;
    fn INTL_ICU_VERSION() -> &'static str {
        super::ffi::icu_version()
    }
    fn INTL_ICU_DATA_VERSION() -> &'static str {
        super::ffi::icu_data_version()
    }
    pub(crate) const GRAPHEME_EXTR_COUNT: i64 = 0;
    pub(crate) const GRAPHEME_EXTR_MAXBYTES: i64 = 1;
    pub(crate) const GRAPHEME_EXTR_MAXCHARS: i64 = 2;
    pub(crate) const IDNA_DEFAULT: i64 = 48;
    pub(crate) const IDNA_ALLOW_UNASSIGNED: i64 = 1;
    pub(crate) const IDNA_USE_STD3_RULES: i64 = 2;
    pub(crate) const IDNA_CHECK_BIDI: i64 = 4;
    pub(crate) const IDNA_CHECK_CONTEXTJ: i64 = 8;
    pub(crate) const IDNA_NONTRANSITIONAL_TO_ASCII: i64 = 16;
    pub(crate) const IDNA_NONTRANSITIONAL_TO_UNICODE: i64 = 32;
    pub(crate) const INTL_IDNA_VARIANT_UTS46: i64 = 1;
    pub(crate) const IDNA_ERROR_EMPTY_LABEL: i64 = 1;
    pub(crate) const IDNA_ERROR_LABEL_TOO_LONG: i64 = 2;
    pub(crate) const IDNA_ERROR_DOMAIN_NAME_TOO_LONG: i64 = 4;
    pub(crate) const IDNA_ERROR_LEADING_HYPHEN: i64 = 8;
    pub(crate) const IDNA_ERROR_TRAILING_HYPHEN: i64 = 16;
    pub(crate) const IDNA_ERROR_HYPHEN_3_4: i64 = 32;
    pub(crate) const IDNA_ERROR_LEADING_COMBINING_MARK: i64 = 64;
    pub(crate) const IDNA_ERROR_DISALLOWED: i64 = 128;
    pub(crate) const IDNA_ERROR_PUNYCODE: i64 = 256;
    pub(crate) const IDNA_ERROR_LABEL_HAS_DOT: i64 = 512;
    pub(crate) const IDNA_ERROR_INVALID_ACE_LABEL: i64 = 1024;
    pub(crate) const IDNA_ERROR_BIDI: i64 = 2048;
    pub(crate) const IDNA_ERROR_CONTEXTJ: i64 = 4096;
    pub(crate) const ULOC_ACTUAL_LOCALE: i64 = 0;
    pub(crate) const ULOC_VALID_LOCALE: i64 = 1;
    pub(crate) const U_USING_FALLBACK_WARNING: i64 = -128;
    pub(crate) const U_ERROR_WARNING_START: i64 = -128;
    pub(crate) const U_USING_DEFAULT_WARNING: i64 = -127;
    pub(crate) const U_SAFECLONE_ALLOCATED_WARNING: i64 = -126;
    pub(crate) const U_STATE_OLD_WARNING: i64 = -125;
    pub(crate) const U_STRING_NOT_TERMINATED_WARNING: i64 = -124;
    pub(crate) const U_SORT_KEY_TOO_SHORT_WARNING: i64 = -123;
    pub(crate) const U_AMBIGUOUS_ALIAS_WARNING: i64 = -122;
    pub(crate) const U_DIFFERENT_UCA_VERSION: i64 = -121;
    pub(crate) const U_ERROR_WARNING_LIMIT: i64 = -119;
    pub(crate) const U_ZERO_ERROR: i64 = 0;
    pub(crate) const U_ILLEGAL_ARGUMENT_ERROR: i64 = 1;
    pub(crate) const U_MISSING_RESOURCE_ERROR: i64 = 2;
    pub(crate) const U_INVALID_FORMAT_ERROR: i64 = 3;
    pub(crate) const U_FILE_ACCESS_ERROR: i64 = 4;
    pub(crate) const U_INTERNAL_PROGRAM_ERROR: i64 = 5;
    pub(crate) const U_MESSAGE_PARSE_ERROR: i64 = 6;
    pub(crate) const U_MEMORY_ALLOCATION_ERROR: i64 = 7;
    pub(crate) const U_INDEX_OUTOFBOUNDS_ERROR: i64 = 8;
    pub(crate) const U_PARSE_ERROR: i64 = 9;
    pub(crate) const U_INVALID_CHAR_FOUND: i64 = 10;
    pub(crate) const U_TRUNCATED_CHAR_FOUND: i64 = 11;
    pub(crate) const U_ILLEGAL_CHAR_FOUND: i64 = 12;
    pub(crate) const U_INVALID_TABLE_FORMAT: i64 = 13;
    pub(crate) const U_INVALID_TABLE_FILE: i64 = 14;
    pub(crate) const U_BUFFER_OVERFLOW_ERROR: i64 = 15;
    pub(crate) const U_UNSUPPORTED_ERROR: i64 = 16;
    pub(crate) const U_RESOURCE_TYPE_MISMATCH: i64 = 17;
    pub(crate) const U_ILLEGAL_ESCAPE_SEQUENCE: i64 = 18;
    pub(crate) const U_UNSUPPORTED_ESCAPE_SEQUENCE: i64 = 19;
    pub(crate) const U_NO_SPACE_AVAILABLE: i64 = 20;
    pub(crate) const U_CE_NOT_FOUND_ERROR: i64 = 21;
    pub(crate) const U_PRIMARY_TOO_LONG_ERROR: i64 = 22;
    pub(crate) const U_STATE_TOO_OLD_ERROR: i64 = 23;
    pub(crate) const U_TOO_MANY_ALIASES_ERROR: i64 = 24;
    pub(crate) const U_ENUM_OUT_OF_SYNC_ERROR: i64 = 25;
    pub(crate) const U_INVARIANT_CONVERSION_ERROR: i64 = 26;
    pub(crate) const U_INVALID_STATE_ERROR: i64 = 27;
    pub(crate) const U_COLLATOR_VERSION_MISMATCH: i64 = 28;
    pub(crate) const U_USELESS_COLLATOR_ERROR: i64 = 29;
    pub(crate) const U_NO_WRITE_PERMISSION: i64 = 30;
    pub(crate) const U_STANDARD_ERROR_LIMIT: i64 = 32;
    pub(crate) const U_BAD_VARIABLE_DEFINITION: i64 = 65536;
    pub(crate) const U_PARSE_ERROR_START: i64 = 65536;
    pub(crate) const U_MALFORMED_RULE: i64 = 65537;
    pub(crate) const U_MALFORMED_SET: i64 = 65538;
    pub(crate) const U_MALFORMED_SYMBOL_REFERENCE: i64 = 65539;
    pub(crate) const U_MALFORMED_UNICODE_ESCAPE: i64 = 65540;
    pub(crate) const U_MALFORMED_VARIABLE_DEFINITION: i64 = 65541;
    pub(crate) const U_MALFORMED_VARIABLE_REFERENCE: i64 = 65542;
    pub(crate) const U_MISMATCHED_SEGMENT_DELIMITERS: i64 = 65543;
    pub(crate) const U_MISPLACED_ANCHOR_START: i64 = 65544;
    pub(crate) const U_MISPLACED_CURSOR_OFFSET: i64 = 65545;
    pub(crate) const U_MISPLACED_QUANTIFIER: i64 = 65546;
    pub(crate) const U_MISSING_OPERATOR: i64 = 65547;
    pub(crate) const U_MISSING_SEGMENT_CLOSE: i64 = 65548;
    pub(crate) const U_MULTIPLE_ANTE_CONTEXTS: i64 = 65549;
    pub(crate) const U_MULTIPLE_CURSORS: i64 = 65550;
    pub(crate) const U_MULTIPLE_POST_CONTEXTS: i64 = 65551;
    pub(crate) const U_TRAILING_BACKSLASH: i64 = 65552;
    pub(crate) const U_UNDEFINED_SEGMENT_REFERENCE: i64 = 65553;
    pub(crate) const U_UNDEFINED_VARIABLE: i64 = 65554;
    pub(crate) const U_UNQUOTED_SPECIAL: i64 = 65555;
    pub(crate) const U_UNTERMINATED_QUOTE: i64 = 65556;
    pub(crate) const U_RULE_MASK_ERROR: i64 = 65557;
    pub(crate) const U_MISPLACED_COMPOUND_FILTER: i64 = 65558;
    pub(crate) const U_MULTIPLE_COMPOUND_FILTERS: i64 = 65559;
    pub(crate) const U_INVALID_RBT_SYNTAX: i64 = 65560;
    pub(crate) const U_INVALID_PROPERTY_PATTERN: i64 = 65561;
    pub(crate) const U_MALFORMED_PRAGMA: i64 = 65562;
    pub(crate) const U_UNCLOSED_SEGMENT: i64 = 65563;
    pub(crate) const U_ILLEGAL_CHAR_IN_SEGMENT: i64 = 65564;
    pub(crate) const U_VARIABLE_RANGE_EXHAUSTED: i64 = 65565;
    pub(crate) const U_VARIABLE_RANGE_OVERLAP: i64 = 65566;
    pub(crate) const U_ILLEGAL_CHARACTER: i64 = 65567;
    pub(crate) const U_INTERNAL_TRANSLITERATOR_ERROR: i64 = 65568;
    pub(crate) const U_INVALID_ID: i64 = 65569;
    pub(crate) const U_INVALID_FUNCTION: i64 = 65570;
    pub(crate) const U_PARSE_ERROR_LIMIT: i64 = 65571;
    pub(crate) const U_UNEXPECTED_TOKEN: i64 = 65792;
    pub(crate) const U_FMT_PARSE_ERROR_START: i64 = 65792;
    pub(crate) const U_MULTIPLE_DECIMAL_SEPARATORS: i64 = 65793;
    #[php(deprecated(since = "8.3", message = "use U_MULTIPLE_DECIMAL_SEPARATORS instead"))]
    pub(crate) const U_MULTIPLE_DECIMAL_SEPERATORS: i64 = 65793;
    pub(crate) const U_MULTIPLE_EXPONENTIAL_SYMBOLS: i64 = 65794;
    pub(crate) const U_MALFORMED_EXPONENTIAL_PATTERN: i64 = 65795;
    pub(crate) const U_MULTIPLE_PERCENT_SYMBOLS: i64 = 65796;
    pub(crate) const U_MULTIPLE_PERMILL_SYMBOLS: i64 = 65797;
    pub(crate) const U_MULTIPLE_PAD_SPECIFIERS: i64 = 65798;
    pub(crate) const U_PATTERN_SYNTAX_ERROR: i64 = 65799;
    pub(crate) const U_ILLEGAL_PAD_POSITION: i64 = 65800;
    pub(crate) const U_UNMATCHED_BRACES: i64 = 65801;
    pub(crate) const U_UNSUPPORTED_PROPERTY: i64 = 65802;
    pub(crate) const U_UNSUPPORTED_ATTRIBUTE: i64 = 65803;
    pub(crate) const U_FMT_PARSE_ERROR_LIMIT: i64 = 65825;
    pub(crate) const U_BRK_INTERNAL_ERROR: i64 = 66048;
    pub(crate) const U_BRK_ERROR_START: i64 = 66048;
    pub(crate) const U_BRK_HEX_DIGITS_EXPECTED: i64 = 66049;
    pub(crate) const U_BRK_SEMICOLON_EXPECTED: i64 = 66050;
    pub(crate) const U_BRK_RULE_SYNTAX: i64 = 66051;
    pub(crate) const U_BRK_UNCLOSED_SET: i64 = 66052;
    pub(crate) const U_BRK_ASSIGN_ERROR: i64 = 66053;
    pub(crate) const U_BRK_VARIABLE_REDFINITION: i64 = 66054;
    pub(crate) const U_BRK_MISMATCHED_PAREN: i64 = 66055;
    pub(crate) const U_BRK_NEW_LINE_IN_QUOTED_STRING: i64 = 66056;
    pub(crate) const U_BRK_UNDEFINED_VARIABLE: i64 = 66057;
    pub(crate) const U_BRK_INIT_ERROR: i64 = 66058;
    pub(crate) const U_BRK_RULE_EMPTY_SET: i64 = 66059;
    pub(crate) const U_BRK_UNRECOGNIZED_OPTION: i64 = 66060;
    pub(crate) const U_BRK_MALFORMED_RULE_TAG: i64 = 66061;
    pub(crate) const U_BRK_ERROR_LIMIT: i64 = 66062;
    pub(crate) const U_REGEX_INTERNAL_ERROR: i64 = 66304;
    pub(crate) const U_REGEX_ERROR_START: i64 = 66304;
    pub(crate) const U_REGEX_RULE_SYNTAX: i64 = 66305;
    pub(crate) const U_REGEX_INVALID_STATE: i64 = 66306;
    pub(crate) const U_REGEX_BAD_ESCAPE_SEQUENCE: i64 = 66307;
    pub(crate) const U_REGEX_PROPERTY_SYNTAX: i64 = 66308;
    pub(crate) const U_REGEX_UNIMPLEMENTED: i64 = 66309;
    pub(crate) const U_REGEX_MISMATCHED_PAREN: i64 = 66310;
    pub(crate) const U_REGEX_NUMBER_TOO_BIG: i64 = 66311;
    pub(crate) const U_REGEX_BAD_INTERVAL: i64 = 66312;
    pub(crate) const U_REGEX_MAX_LT_MIN: i64 = 66313;
    pub(crate) const U_REGEX_INVALID_BACK_REF: i64 = 66314;
    pub(crate) const U_REGEX_INVALID_FLAG: i64 = 66315;
    pub(crate) const U_REGEX_LOOK_BEHIND_LIMIT: i64 = 66316;
    pub(crate) const U_REGEX_SET_CONTAINS_STRING: i64 = 66317;
    pub(crate) const U_REGEX_ERROR_LIMIT: i64 = 66326;
    pub(crate) const U_IDNA_PROHIBITED_ERROR: i64 = 66560;
    pub(crate) const U_IDNA_ERROR_START: i64 = 66560;
    pub(crate) const U_IDNA_UNASSIGNED_ERROR: i64 = 66561;
    pub(crate) const U_IDNA_CHECK_BIDI_ERROR: i64 = 66562;
    pub(crate) const U_IDNA_STD3_ASCII_RULES_ERROR: i64 = 66563;
    pub(crate) const U_IDNA_ACE_PREFIX_ERROR: i64 = 66564;
    pub(crate) const U_IDNA_VERIFICATION_ERROR: i64 = 66565;
    pub(crate) const U_IDNA_LABEL_TOO_LONG_ERROR: i64 = 66566;
    pub(crate) const U_IDNA_ZERO_LENGTH_LABEL_ERROR: i64 = 66567;
    pub(crate) const U_IDNA_DOMAIN_NAME_TOO_LONG_ERROR: i64 = 66568;
    pub(crate) const U_IDNA_ERROR_LIMIT: i64 = 66569;
    pub(crate) const U_STRINGPREP_PROHIBITED_ERROR: i64 = 66560;
    pub(crate) const U_STRINGPREP_UNASSIGNED_ERROR: i64 = 66561;
    pub(crate) const U_STRINGPREP_CHECK_BIDI_ERROR: i64 = 66562;
    pub(crate) const U_ERROR_LIMIT: i64 = 66818;
}

/// `zend_intl_globals`, request-scoped (ADR-0005): the request's
/// `intl_error`. `default_locale`, `error_level` and `use_exceptions`
/// are the ini entries themselves, overridden while a constructor runs.
#[derive(Default)]
pub(crate) struct State {
    error: IntlError,
    /// Inside a constructor's [`error::constructing`]: php's
    /// `INTL_G(use_exceptions) = true; INTL_G(error_level) = 0;`.
    constructing: bool,
}

/// The extension value (design §7.1): `intl_module_entry`, its ini
/// entries, its constants, its functions in the stub's order and its
/// classes in `PHP_MINIT_FUNCTION(intl)`'s.
///
/// ```php
/// var_dump(extension_loaded('intl'), class_exists('IntlException'), phpversion('intl') === PHP_VERSION);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
pub fn extension() -> Extension {
    let builder = ExtensionBuilder::default()
        .ini(vec![
            ini(DEFAULT_LOCALE, None),
            ini(ERROR_LEVEL, Some(b"0")),
            ini(USE_EXCEPTIONS, Some(b"0")),
        ])
        .on_modify(DEFAULT_LOCALE, on_update_default_locale)
        .on_modify(ERROR_LEVEL, on_update_error_level)
        .constants(constants::DECLARED);
    // `php_intl.stub.php`'s functions, block by block: intlcal_* and
    // intlgregcal_*, collator_*, intl_*, datefmt_*, numfmt_*,
    // grapheme_*, idn_*, locale_*, msgfmt_*, normalizer_*,
    // resourcebundle_*, intltz_*, transliterator_*.
    let builder = calendar::functions(builder);
    let builder = collator::functions(builder);
    let builder = builder.functions(&[
        intl_get_error_code::DECLARED,
        intl_get_error_message::DECLARED,
        intl_is_failure::DECLARED,
        intl_error_name::DECLARED,
    ]);
    let builder = dateformat::functions(builder);
    let builder = formatter::functions(builder);
    let builder = grapheme::functions(builder);
    let builder = idn::functions(builder);
    let builder = locale::functions(builder);
    let builder = msgformat::functions(builder);
    let builder = normalizer::functions(builder);
    let builder = resourcebundle::functions(builder);
    let builder = timezone::functions(builder);
    let builder = transliterator::functions(builder);
    // The classes in `PHP_MINIT_FUNCTION(intl)`'s order: Collator,
    // NumberFormatter, IntlListFormatter, Normalizer, Locale,
    // MessageFormatter, IntlDateFormatter, IntlDatePatternGenerator,
    // ResourceBundle, Transliterator, IntlTimeZone, IntlCalendar,
    // IntlGregorianCalendar, Spoofchecker, IntlException, IntlIterator,
    // IntlBreakIterator, IntlRuleBasedBreakIterator,
    // IntlCodePointBreakIterator, IntlPartsIterator, UConverter, IntlChar.
    let builder = builder
        .class::<collator::Collator>()
        .class::<formatter::NumberFormatter>()
        .class::<listformatter::IntlListFormatter>()
        .class::<normalizer::Normalizer>()
        .class::<locale::Locale>()
        .class::<msgformat::MessageFormatter>()
        .class::<dateformat::IntlDateFormatter>()
        .class::<datepatterngenerator::IntlDatePatternGenerator>()
        .class::<resourcebundle::ResourceBundle>()
        .class::<transliterator::Transliterator>()
        .class::<timezone::IntlTimeZone>()
        .class::<calendar::IntlCalendar>()
        .class::<calendar::IntlGregorianCalendar>()
        .class::<spoofchecker::Spoofchecker>()
        .class::<IntlException>()
        .class::<iterator::IntlIterator>()
        .class::<breakiterator::IntlBreakIterator>()
        .class::<breakiterator::IntlRuleBasedBreakIterator>()
        .class::<breakiterator::IntlCodePointBreakIterator>()
        .class::<breakiterator::IntlPartsIterator>()
        .class::<converter::UConverter>()
        .class::<uchar::IntlChar>()
        .class_constants(b"IntlChar", uchar::constants());
    builder
        .request_state::<State>()
        .rshutdown(rshutdown)
        .post_deactivate(post_deactivate)
        .engine_state(throwables::Throwables::start)
        .info(intl_info)
        .build("intl")
}

/// `PHP_RSHUTDOWN_FUNCTION(intl)`'s `intl_error_reset(NULL)`: the
/// request's code zero and its message freed.
fn rshutdown(req: &mut Request) {
    error::reset(req);
}

/// The request's message an object freed after `RSHUTDOWN` set, as a
/// `UConverter`'s close callbacks may, freed with the request's heap,
/// where php leaves it to `zend_mm`'s shutdown.
fn post_deactivate(req: &mut Request) {
    error::reset(req);
}

/// One `PHP_INI_ALL` entry.
fn ini(name: &'static [u8], default: Option<&'static [u8]>) -> IniDecl {
    IniDecl {
        name,
        default,
        user: true,
        per_dir: true,
        system: true,
    }
}

/// `OnUpdateStringUnempty`: an empty locale refused.
///
/// ```php
/// var_dump(ini_set('intl.default_locale', ''), ini_set('intl.default_locale', 'fr_FR'), ini_get('intl.default_locale'));
/// ```
/// ```expect
/// bool(false)
/// string(0) ""
/// string(5) "fr_FR"
/// ```
#[expect(
    clippy::unnecessary_wraps,
    reason = "the `on_modify` slot's shape, which a handler that throws needs"
)]
fn on_update_default_locale(_req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    Ok(value.is_none_or(|v| !v.is_empty()))
}

/// `OnUpdateErrorLevel`: the level read as a quantity, and a level
/// other than zero deprecated.
///
/// ```php
/// ini_set('intl.error_level', E_WARNING);
/// var_dump(ini_get('intl.error_level'));
/// ```
/// ```expect
/// Deprecated: ini_set(): Using a value different than 0 for intl.error_level is deprecated, as the intl.error_level INI setting is deprecated. Instead the intl.use_exceptions INI setting should be enabled to throw exceptions on errors or intl_get_error_code()/intl_get_error_message() should be used to manually deal with errors in %s on line 2
/// string(1) "2"
/// ```
///
/// The level is `zend_ini_parse_quantity_warn`'s, which warns where it
/// interprets the value:
///
/// ```php
/// var_dump(ini_set('intl.error_level', '0z'), ini_get('intl.error_level'));
/// ```
/// ```expect
/// Warning: Invalid "intl.error_level" setting. Invalid prefix "0z", interpreting as "0" for backwards compatibility in %s on line 2
/// string(1) "0"
/// string(2) "0z"
/// ```
#[expect(
    clippy::unnecessary_wraps,
    reason = "the `on_modify` slot's shape, which a handler that throws needs"
)]
fn on_update_error_level(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let level = req
        .ini_mut()
        .parse_quantity_warn(b"intl.error_level", value.unwrap_or_default());
    if level != 0 {
        docref(
            req,
            Level::Deprecated,
            "Using a value different than 0 for intl.error_level is deprecated, as the intl.error_level INI setting is deprecated. Instead the intl.use_exceptions INI setting should be enabled to throw exceptions on errors or intl_get_error_code()/intl_get_error_message() should be used to manually deal with errors",
        );
    }
    Ok(true)
}

/// `PHP_MINFO_FUNCTION(intl)`: the support row, ICU's version, its data's,
/// the time zone data's and Unicode's, then the ini entries.
///
/// ```php
/// ob_start(); (new ReflectionExtension('intl'))->info(); $info = ob_get_clean();
/// echo preg_replace('/=> [0-9][0-9a-z.]*$/m', '=> X', $info);
/// ```
/// ```expect
/// intl
///
/// Internationalization support => enabled
/// ICU version => X
/// ICU Data version => X
/// ICU TZData version => X
/// ICU Unicode version => X
///
/// Directive => Local Value => Master Value
/// intl.default_locale => no value => no value
/// intl.error_level => 0 => X
/// intl.use_exceptions => Off => Off
/// ```
fn intl_info(req: &mut Request) {
    let mut rows: Vec<[&str; 2]> = vec![
        ["Internationalization support", "enabled"],
        ["ICU version", ffi::icu_version()],
        ["ICU Data version", ffi::icu_data_version()],
    ];
    if let Some(version) = ffi::tz_data_version() {
        rows.push(["ICU TZData version", version]);
    }
    rows.push(["ICU Unicode version", ffi::unicode_version()]);
    let rows: Vec<&[&str]> = rows.iter().map(<[&str; 2]>::as_slice).collect();
    req.extensions_mut().info_table(&rows);
    // `DISPLAY_INI_ENTRIES()`: each entry's request value beside its
    // master, `intl.use_exceptions` through the boolean displayer.
    let rows: Vec<[String; 3]> = req
        .ini()
        .entries()
        .iter()
        .filter(|entry| [DEFAULT_LOCALE, ERROR_LEVEL, USE_EXCEPTIONS].contains(&entry.decl.name))
        .map(|entry| {
            let boolean = entry.decl.name == USE_EXCEPTIONS;
            let shown = |value: Option<&[u8]>| {
                let value = value.unwrap_or_default();
                match (boolean, Ini::parse_bool(value)) {
                    (true, true) => "On".to_owned(),
                    (true, false) => "Off".to_owned(),
                    (false, _) => String::from_utf8_lossy(value).into_owned(),
                }
            };
            [
                String::from_utf8_lossy(entry.decl.name).into_owned(),
                shown(entry.value.as_deref()),
                shown(entry.master.as_deref()),
            ]
        })
        .collect();
    let cells: Vec<[&str; 3]> = rows
        .iter()
        .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
        .collect();
    req.ini_mut().info_ini_table(&cells);
}

/// `intl_locale_get_default`: `intl.default_locale` where it is set,
/// ICU's default otherwise.
pub(crate) fn default_locale(req: &Request) -> Vec<u8> {
    match req.ini().get(DEFAULT_LOCALE) {
        Some(locale) if !locale.is_empty() => locale.to_vec(),
        _ => ffi::default_locale().as_bytes().to_vec(),
    }
}

/// Core's `Exception`, named for `extends`.
#[php_class(extern)]
pub(crate) struct Exception;

/// `class IntlException extends Exception`, what an intl error throws
/// under `intl.use_exceptions`.
///
/// ```php
/// ini_set('intl.use_exceptions', 1);
/// try { (new ReflectionClass('IntlException'))->newInstance('m', 3)->getMessage(); throw new IntlException('x'); } catch (Exception $e) { var_dump(get_class($e), get_parent_class($e)); }
/// ```
/// ```expect
/// string(13) "IntlException"
/// string(9) "Exception"
/// ```
#[php_class(extends(Exception))]
pub(crate) struct IntlException;

#[php_class]
impl IntlException {}

/// A `zend_long` cast to `int32_t`, C's truncation to its low 32 bits.
pub(crate) fn truncated(n: i64) -> i32 {
    let b = n.to_le_bytes();
    i32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// A `zend_long` cast to `UErrorCode`.
pub(crate) fn code_of(n: i64) -> Code {
    Code(truncated(n))
}

/// `intl_get_error_code(): int`: the request's last code.
///
/// ```php
/// var_dump(intl_get_error_code());
/// try { intl_get_error_code(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(0)
/// intl_get_error_code() expects exactly 0 arguments, 1 given
/// ```
#[php_function]
fn intl_get_error_code(req: &mut Request) -> i64 {
    i64::from(error::code(req).0)
}

/// `intl_get_error_message(): string`: the request's message and its
/// code's name.
///
/// ```php
/// var_dump(intl_get_error_message());
/// ```
/// ```expect
/// string(12) "U_ZERO_ERROR"
/// ```
#[php_function]
fn intl_get_error_message(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let message = error::message(req);
    PhpString::new(req, &message)
}

/// `intl_is_failure(int $errorCode): bool`: `U_FAILURE`, over the long.
///
/// ```php
/// var_dump(intl_is_failure(U_ZERO_ERROR), intl_is_failure(U_USING_FALLBACK_WARNING), intl_is_failure(U_ILLEGAL_ARGUMENT_ERROR), intl_is_failure(PHP_INT_MAX));
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
#[allow(non_snake_case)]
fn intl_is_failure(errorCode: i64) -> bool {
    errorCode > 0
}

/// `intl_error_name(int $errorCode): string`: `u_errorName` of the
/// code, cast to `UErrorCode`.
///
/// ```php
/// var_dump(intl_error_name(U_ZERO_ERROR), intl_error_name(U_INVALID_CHAR_FOUND), intl_error_name(-128), intl_error_name(99999), intl_error_name((1 << 32) + 1));
/// ```
/// ```expect
/// string(12) "U_ZERO_ERROR"
/// string(20) "U_INVALID_CHAR_FOUND"
/// string(24) "U_USING_FALLBACK_WARNING"
/// string(18) "[BOGUS UErrorCode]"
/// string(24) "U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function]
#[allow(non_snake_case)]
fn intl_error_name(req: &mut Request, errorCode: i64) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, code_of(errorCode).name().as_bytes())
}
