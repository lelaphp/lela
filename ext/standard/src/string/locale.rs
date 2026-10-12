//! The locale natives of `string.c`, `setlocale`, `localeconv`,
//! `nl_langinfo` and `strcoll`, over a request's own locale (design
//! §5.14, revision 40). php's `setlocale` moves the process's locale,
//! which one request per process makes safe; here `setlocale()` builds a
//! `locale_t` with `newlocale(3)` from the categories it names, starting
//! from the one the thread runs under, and installs it on the thread
//! serving the request with `uselocale(3)`, so what reads the locale,
//! here and in `*printf`'s `%f`, `SORT_LOCALE_STRING` and ctype, is the
//! C library's own code under this request's locale and no other's. The
//! request keeps each category's name beside it, as the host's
//! `setlocale(3)` reports it, starting where php leaves it after
//! `zend_reset_lc_ctype_locale`, `C.UTF-8` for `LC_CTYPE` and `C` for
//! the rest; at the request's end the thread's own locale is put back
//! and the request's freed, as `basic_functions`'s `RSHUTDOWN` sets the
//! process's back to `C`.

use std::cmp::Ordering;
use std::ffi::{CString, c_char};
use std::ops::ControlFlow;

use php_ext::abi::{OrFalse, Untyped, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::{c_str, with_string};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef};

use super::constants::{
    self, LC_ALL, LC_COLLATE, LC_CTYPE, LC_MESSAGES, LC_MONETARY, LC_NUMERIC, LC_TIME,
};
use crate::ffi::locale::{self as c, Category, Installed};

/// `LC_ALL` and the six categories, the values of the pinned build's
/// `<locale.h>`, in `setlocale(LC_ALL, NULL)`'s composite order.
const CATEGORIES: [(&[u8], i64); 7] = [
    (b"LC_CTYPE", LC_CTYPE),
    (b"LC_NUMERIC", LC_NUMERIC),
    (b"LC_TIME", LC_TIME),
    (b"LC_COLLATE", LC_COLLATE),
    (b"LC_MONETARY", LC_MONETARY),
    (b"LC_ALL", LC_ALL),
    (b"LC_MESSAGES", LC_MESSAGES),
];

/// The category a `<locale.h>` value of the pinned build names, `LC_ALL`
/// aside.
fn category(value: usize) -> Option<Category> {
    let value = i64::try_from(value).ok()?;
    Some(match value {
        LC_COLLATE => Category::Collate,
        LC_CTYPE => Category::Ctype,
        LC_MONETARY => Category::Monetary,
        LC_NUMERIC => Category::Numeric,
        LC_TIME => Category::Time,
        LC_MESSAGES => Category::Messages,
        _ => return None,
    })
}

/// `nl_langinfo`'s items: the pinned build's `<langinfo.h>` value each
/// constant carries beside the item the host's C library knows it by.
macro_rules! items {
    ($($name:ident),* $(,)?) => {
        [$((constants::$name, libc::$name)),*]
    };
}
const LANGINFO: [(i64, libc::nl_item); 57] = items!(
    CODESET,
    D_T_FMT,
    D_FMT,
    T_FMT,
    T_FMT_AMPM,
    AM_STR,
    PM_STR,
    DAY_1,
    DAY_2,
    DAY_3,
    DAY_4,
    DAY_5,
    DAY_6,
    DAY_7,
    ABDAY_1,
    ABDAY_2,
    ABDAY_3,
    ABDAY_4,
    ABDAY_5,
    ABDAY_6,
    ABDAY_7,
    MON_1,
    MON_2,
    MON_3,
    MON_4,
    MON_5,
    MON_6,
    MON_7,
    MON_8,
    MON_9,
    MON_10,
    MON_11,
    MON_12,
    ABMON_1,
    ABMON_2,
    ABMON_3,
    ABMON_4,
    ABMON_5,
    ABMON_6,
    ABMON_7,
    ABMON_8,
    ABMON_9,
    ABMON_10,
    ABMON_11,
    ABMON_12,
    ERA,
    ERA_D_FMT,
    ERA_D_T_FMT,
    ERA_T_FMT,
    ALT_DIGITS,
    RADIXCHAR,
    THOUSEP,
    YESEXPR,
    NOEXPR,
    YESSTR,
    NOSTR,
    CRNCYSTR,
);

/// A request's locale: the `locale_t` installed on the thread serving it
/// once `setlocale()` has built one. Each category's name is the
/// request's environment's (design §4.4, revision 56).
#[derive(Debug, Default)]
pub(super) struct Locale {
    installed: Option<Installed>,
}

/// One category's name, `C.UTF-8` for `LC_CTYPE` and `C` for the rest
/// until `setlocale()` moves it.
fn name(req: &Request, category: usize) -> Vec<u8> {
    req.locale_name(category).map_or_else(
        || {
            if category == 2 {
                b"C.UTF-8".to_vec()
            } else {
                b"C".to_vec()
            }
        },
        <[u8]>::to_vec,
    )
}

/// `setlocale(category, NULL)`: for `LC_ALL` the one name when every
/// category agrees, else the six in order joined by `/`.
fn query(req: &Request, category: usize) -> Vec<u8> {
    if category != 0 {
        return name(req, category);
    }
    let names: Vec<Vec<u8>> = (1..7).map(|c| name(req, c)).collect();
    if names.iter().all(|n| *n == names[0]) {
        return names[0].clone();
    }
    names.join(&b'/')
}

impl Locale {
    /// Install `locale` on the thread from here to the request's end, in
    /// place of the one installed before it.
    fn install(&mut self, locale: c::Locale) {
        match &mut self.installed {
            Some(installed) => installed.replace(locale),
            None => self.installed = Some(Installed::install(locale)),
        }
    }
}

/// The name `setlocale(category, "")` reads for one category off the
/// request's environment: `LC_ALL`, then the category's variable, then
/// `LANG`, then `C`. A name the C library refuses fails the call, as php
/// built without libintl fails it; the pinned oracle's libintl falls back
/// category by category instead (`trace:setlocale-libintl-fallback`,
/// #1126), which `tests/locale.rs` holds the engine's side of.
fn from_environment(req: &Request, category: usize) -> Vec<u8> {
    let variable = CATEGORIES
        .iter()
        .find(|(_, value)| usize::try_from(*value).ok() == Some(category))
        .map_or(&b"LC_ALL"[..], |(name, _)| name);
    [&b"LC_ALL"[..], variable, b"LANG"]
        .iter()
        .find_map(|name| req.env(name).filter(|v| !v.is_empty()).map(<[u8]>::to_vec))
        .unwrap_or_else(|| b"C".to_vec())
}

/// The bytes up to the first NUL, what the C library reads of a
/// `zend_string`'s value, as a C string.
fn c_string(bytes: &[u8]) -> CString {
    CString::new(c_str(bytes)).unwrap_or_default()
}

/// `try_setlocale_str(cat, loc)`: the name answered, or `None` where
/// the C library knows no such locale. The locale is built whole before
/// it is installed, every category `LC_ALL` names or none.
fn try_setlocale(req: &mut Request, category: i64, loc: &[u8]) -> Option<Vec<u8>> {
    let category = usize::try_from(category).ok().filter(|c| *c < 7)?;
    if loc == b"0" {
        return Some(query(req, category));
    }
    if loc.len() >= 255 {
        docref(req, Level::Warning, "Specified locale name is too long");
        return None;
    }
    let targets: Vec<usize> = if category == 0 {
        (1..7).collect()
    } else {
        vec![category]
    };
    let mut built = c::Locale::current()?;
    let mut names = Vec::with_capacity(targets.len());
    for target in &targets {
        let name = c_string(&if loc.is_empty() {
            from_environment(req, *target)
        } else {
            loc.to_vec()
        });
        built = built.with(self::category(*target)?, &name).ok()?;
        names.push(name.into_bytes());
    }
    req.state::<crate::State>().string.locale.install(built);
    for (target, name) in targets.iter().zip(names) {
        req.set_locale_name(*target, name);
    }
    let retval = query(req, category);
    if category == 0 || category == 2 {
        req.set_ctype_locale((retval != b"C").then(|| retval.clone()));
    }
    Some(retval)
}

/// `setlocale(int $category, $locales, ...$rest): string|false`: each
/// candidate in turn, an array's elements in order, until one takes;
/// `"0"` asks for the current name, `""` and `null` for the one the
/// environment names.
///
/// ```php
/// var_dump(setlocale(LC_ALL, "0"), setlocale(LC_ALL, "C"), setlocale(LC_ALL, "0"), setlocale(LC_CTYPE, "0"), setlocale(LC_ALL, ""), setlocale(LC_ALL, null), setlocale(LC_ALL, ["xx_XX", "C"]), setlocale(LC_ALL, "xx_XX", "yy", "POSIX"), setlocale(LC_ALL, "0"), setlocale(LC_NUMERIC, "C"), setlocale(LC_ALL, "0"), setlocale(LC_ALL, "xx_XX"), setlocale(LC_ALL, []), setlocale(100, "C"));
/// var_dump(setlocale(LC_ALL, str_repeat("a", 255)));
/// try { setlocale(LC_ALL, new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { setlocale(LC_ALL); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(17) "C/C.UTF-8/C/C/C/C"
/// string(1) "C"
/// string(1) "C"
/// string(1) "C"
/// string(1) "C"
/// string(1) "C"
/// string(1) "C"
/// string(5) "POSIX"
/// string(5) "POSIX"
/// string(1) "C"
/// string(31) "POSIX/POSIX/POSIX/C/POSIX/POSIX"
/// bool(false)
/// bool(false)
/// bool(false)
///
/// Warning: setlocale(): Specified locale name is too long in %s on line 3
/// bool(false)
/// setlocale(): Argument #2 ($locales) must be of type array|string|null, stdClass given
/// setlocale() expects at least 2 arguments, 1 given
/// ```
///
/// Any name the host's C library knows is a locale a category moves to,
/// and the name is what it answers, `LC_ALL` over categories that differ
/// joined as the host joins them:
///
/// ```php
/// var_dump(setlocale(LC_ALL, "de_DE"), setlocale(LC_NUMERIC, "is_IS", "is_IS.UTF-8"), setlocale(LC_ALL, "0"), setlocale(LC_COLLATE, "0"));
/// var_dump(setlocale(LC_ALL, ["xx_XX", "fr_FR.ISO8859-1", "fr_FR"]), setlocale(LC_CTYPE, "de_DE.UTF-8"), setlocale(LC_ALL, "0"), setlocale(LC_ALL, "de_DE\0junk"));
/// putenv("LC_ALL="); putenv("LC_TIME=fr_FR.UTF-8"); putenv("LANG=de_DE.UTF-8");
/// var_dump(setlocale(LC_TIME, ""), setlocale(LC_ALL, null), setlocale(LC_ALL, "0"));
/// var_dump(setlocale(LC_ALL, "C"), setlocale(LC_ALL, "0"));
/// ```
/// ```expect locale=de_DE locale=is_IS locale=fr_FR.ISO8859-1 locale=de_DE.UTF-8 locale=fr_FR.UTF-8
/// string(5) "de_DE"
/// string(5) "is_IS"
/// string(35) "de_DE/de_DE/de_DE/is_IS/de_DE/de_DE"
/// string(5) "de_DE"
/// string(15) "fr_FR.ISO8859-1"
/// string(11) "de_DE.UTF-8"
/// string(91) "fr_FR.ISO8859-1/de_DE.UTF-8/fr_FR.ISO8859-1/fr_FR.ISO8859-1/fr_FR.ISO8859-1/fr_FR.ISO8859-1"
/// string(5) "de_DE"
/// string(11) "fr_FR.UTF-8"
/// string(71) "de_DE.UTF-8/de_DE.UTF-8/de_DE.UTF-8/de_DE.UTF-8/fr_FR.UTF-8/de_DE.UTF-8"
/// string(71) "de_DE.UTF-8/de_DE.UTF-8/de_DE.UTF-8/de_DE.UTF-8/fr_FR.UTF-8/de_DE.UTF-8"
/// string(1) "C"
/// string(1) "C"
/// ```
#[php_function]
pub(super) fn setlocale(
    req: &mut Request,
    category: i64,
    locales: Untyped<Value>,
    rest: Variadic<Untyped<Value>>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let given: Vec<Value> = std::iter::once(locales.0)
        .chain(rest.0.into_iter().map(|rest| rest.0))
        .collect();
    let mut candidates: Vec<Value> = Vec::with_capacity(given.len());
    for (value, position) in given.into_iter().zip(2u32..) {
        // Each one an array, or `zend_parse_arg_str`'s string or null, past
        // `Z_PARAM_VARIADIC('+')`, where the stub declares `$locales,
        // ...$rest` untyped.
        let ty = php_ext::decl::Type::Union(vec![
            php_ext::decl::Type::Array,
            php_ext::decl::Type::String,
            php_ext::decl::Type::Null,
        ]);
        candidates.push(php_ext::coerce::coerce_param(req, &value, &ty, position)?);
    }
    for candidate in candidates {
        let names: Vec<Vec<u8>> = match candidate.as_ref() {
            ValueRef::Array(array) => {
                let mut names = Vec::new();
                // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL(args[i]), elem)`
                // (`Z/ext/standard/string.c:4969`), no owner of the table
                // taken: the argument holds it while `__toString` runs.
                walk::foreach(req, *array, Hold::None, |req, row| -> walk::Step {
                    names.push(with_string(req, row.value(), <[u8]>::to_vec)?);
                    Ok(ControlFlow::Continue(()))
                })?;
                names
            }
            ValueRef::Null => vec![Vec::new()],
            _ => vec![candidate.as_str_bytes().unwrap_or_default().to_vec()],
        };
        for name in names {
            if let Some(answer) = try_setlocale(req, category, &name) {
                return PhpString::new(req, &answer).map(OrFalse::Value);
            }
        }
    }
    Ok(OrFalse::False)
}

/// `localeconv(): array`: the thread's `struct lconv` under the
/// request's locale, its eight strings, its eight `char`s as integers,
/// `CHAR_MAX` where the locale leaves one unset, and each grouping's
/// bytes up to its NUL.
///
/// ```php
/// var_dump(localeconv());
/// ```
/// ```expect
/// array(18) {
///   ["decimal_point"]=>
///   string(1) "."
///   ["thousands_sep"]=>
///   string(0) ""
///   ["int_curr_symbol"]=>
///   string(0) ""
///   ["currency_symbol"]=>
///   string(0) ""
///   ["mon_decimal_point"]=>
///   string(0) ""
///   ["mon_thousands_sep"]=>
///   string(0) ""
///   ["positive_sign"]=>
///   string(0) ""
///   ["negative_sign"]=>
///   string(0) ""
///   ["int_frac_digits"]=>
///   int(127)
///   ["frac_digits"]=>
///   int(127)
///   ["p_cs_precedes"]=>
///   int(127)
///   ["p_sep_by_space"]=>
///   int(127)
///   ["n_cs_precedes"]=>
///   int(127)
///   ["n_sep_by_space"]=>
///   int(127)
///   ["p_sign_posn"]=>
///   int(127)
///   ["n_sign_posn"]=>
///   int(127)
///   ["grouping"]=>
///   array(0) {
///   }
///   ["mon_grouping"]=>
///   array(0) {
///   }
/// }
/// ```
///
/// ```php
/// setlocale(LC_ALL, "de_DE.UTF-8"); $l = localeconv();
/// var_dump($l["decimal_point"], $l["thousands_sep"], $l["int_curr_symbol"], $l["mon_decimal_point"], $l["frac_digits"], $l["p_cs_precedes"], $l["grouping"], $l["mon_grouping"]);
/// setlocale(LC_MONETARY, "C"); var_dump(localeconv()["currency_symbol"], localeconv()["decimal_point"]);
/// ```
/// ```expect locale=de_DE.UTF-8
/// string(1) ","
/// string(1) "."
/// string(4) "EUR "
/// string(1) ","
/// int(2)
/// int(0)
/// array(1) {
///   [0]=>
///   int(3)
/// }
/// array(1) {
///   [0]=>
///   int(3)
/// }
/// string(0) ""
/// string(1) ","
/// ```
///
/// Each string is `add_assoc_string`'s `zend_string_init`, an allocation
/// whatever its length, so the empty values and the one-byte `"."` cost
/// what they cost php, where `ZVAL_STRINGL_FAST` would hand out the
/// interned string:
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, "\n"; }
/// pk(function () { $a = localeconv(); });
/// ```
/// ```expect
/// 3000
/// ```
#[php_function]
pub(super) fn localeconv(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let lconv = c::conventions();
    // php's order (`Z/ext/standard/string.c:5649-5691`): the three arrays
    // first, the two groupings filled, then the rows, the groupings
    // last; what stands while the result's table grows is the peak.
    let mut array = PhpArray::new(req)?;
    let mut groupings = Vec::with_capacity(2);
    for (name, grouping) in [
        (&b"grouping"[..], &lconv.grouping),
        (b"mon_grouping", &lconv.mon_grouping),
    ] {
        let mut list = PhpArray::new(req)?;
        for (at, byte) in (0i64..).zip(grouping) {
            let value = i64::from(c_char::from_ne_bytes([*byte]));
            list.set(req, Owner::index(at), Owner::int(value))?;
        }
        groupings.push((name, list));
    }
    let strings: [(&[u8], &[u8]); 8] = [
        (b"decimal_point", &lconv.decimal_point),
        (b"thousands_sep", &lconv.thousands_sep),
        (b"int_curr_symbol", &lconv.int_curr_symbol),
        (b"currency_symbol", &lconv.currency_symbol),
        (b"mon_decimal_point", &lconv.mon_decimal_point),
        (b"mon_thousands_sep", &lconv.mon_thousands_sep),
        (b"positive_sign", &lconv.positive_sign),
        (b"negative_sign", &lconv.negative_sign),
    ];
    for (name, value) in strings {
        // `add_assoc_string`'s `zend_string_init`, never the interned
        // string `PhpString::new` hands out for one byte or none.
        let value = PhpString::filled(req, value.len(), |bytes| bytes.copy_from_slice(value))?;
        array.set_str(req, name, value.into_value())?;
    }
    let chars: [(&[u8], c_char); 8] = [
        (b"int_frac_digits", lconv.int_frac_digits),
        (b"frac_digits", lconv.frac_digits),
        (b"p_cs_precedes", lconv.p_cs_precedes),
        (b"p_sep_by_space", lconv.p_sep_by_space),
        (b"n_cs_precedes", lconv.n_cs_precedes),
        (b"n_sep_by_space", lconv.n_sep_by_space),
        (b"p_sign_posn", lconv.p_sign_posn),
        (b"n_sign_posn", lconv.n_sign_posn),
    ];
    for (name, value) in chars {
        array.set_str(req, name, Owner::int(i64::from(value)))?;
    }
    for (name, list) in groupings {
        array.set_str(req, name, list.into_value())?;
    }
    Ok(array)
}

/// `nl_langinfo(int $item): string|false`: the C library's answer under
/// the request's locale for one of the items the build defines, and the
/// warning and `false` for any other number.
///
/// ```php
/// var_dump(nl_langinfo(CODESET), nl_langinfo(ABDAY_1), nl_langinfo(MON_12), strtr(nl_langinfo(D_T_FMT), "%", "#"), nl_langinfo(RADIXCHAR), nl_langinfo(ERA), nl_langinfo(YESEXPR));
/// setlocale(LC_ALL, "C"); var_dump(nl_langinfo(CODESET), nl_langinfo(99));
/// ```
/// ```expect
/// string(5) "UTF-8"
/// string(3) "Sun"
/// string(8) "December"
/// string(20) "#a #b #e #H:#M:#S #Y"
/// string(1) "."
/// string(0) ""
/// string(5) "^[yY]"
///
/// Warning: nl_langinfo(): Item '99' is not valid in %s on line 3
/// string(8) "US-ASCII"
/// bool(false)
/// ```
///
/// ```php
/// setlocale(LC_ALL, "fr_FR.ISO8859-1");
/// var_dump(nl_langinfo(CODESET), bin2hex(nl_langinfo(MON_2)), nl_langinfo(ABDAY_1), strtr(nl_langinfo(D_FMT), "%", "#"), nl_langinfo(RADIXCHAR), bin2hex(nl_langinfo(THOUSEP)));
/// setlocale(LC_TIME, "de_DE.UTF-8"); var_dump(nl_langinfo(MON_3), nl_langinfo(RADIXCHAR));
/// ```
/// ```expect locale=fr_FR.ISO8859-1 locale=de_DE.UTF-8
/// string(9) "ISO8859-1"
/// string(14) "66e97672696572"
/// string(4) "dim."
/// string(8) "#d.#m.#Y"
/// string(1) ","
/// string(2) "a0"
/// string(5) "März"
/// string(1) ","
/// ```
#[php_function]
pub(super) fn nl_langinfo(
    req: &mut Request,
    item: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Some((_, host)) = LANGINFO.iter().find(|(id, _)| *id == item) else {
        docref(req, Level::Warning, &format!("Item '{item}' is not valid"));
        return Ok(OrFalse::False);
    };
    PhpString::new(req, &c::langinfo(*host)).map(OrFalse::Value)
}

/// `strcoll(3)` under the request's `LC_COLLATE` over the bytes up to
/// each string's first NUL, what `SORT_LOCALE_STRING` orders by, as
/// `string_locale_compare_function` normalises it.
pub(crate) fn collate(left: &[u8], right: &[u8]) -> Ordering {
    c::strcoll(&c_string(left), &c_string(right)).cmp(&0)
}

/// `strcoll(string $string1, string $string2): int`: the C library's
/// collation under the request's `LC_COLLATE`, over the bytes up to each
/// string's first NUL; under `C` it is `strcmp`, whose answer on the
/// pinned build is the difference of the first bytes that differ.
///
/// ```php
/// var_dump(strcoll("a", "b"), strcoll("b", "a"), strcoll("abc", "abc"), strcoll("a", "c"), strcoll("a", ""), strcoll("\xff", "a"), strcoll("a\0b", "a\0c"));
/// ```
/// ```expect
/// int(-1)
/// int(1)
/// int(0)
/// int(-2)
/// int(97)
/// int(158)
/// int(0)
/// ```
///
/// ```php
/// setlocale(LC_COLLATE, "fr_FR.ISO8859-1");
/// var_dump(strcoll("\xC9cosse", "Ontario") < 0, strcoll("Qu\xE9bec", "Quebed") < 0, strcoll("a", "B") < 0, strcoll("abc", "abc"));
/// ```
/// ```expect locale=fr_FR.ISO8859-1
/// bool(false)
/// bool(false)
/// bool(true)
/// int(0)
/// ```
#[php_function]
pub(super) fn strcoll(string1: PhpString, string2: PhpString) -> i64 {
    let (left, right) = (c_string(string1.as_bytes()), c_string(string2.as_bytes()));
    i64::from(c::strcoll(&left, &right))
}
