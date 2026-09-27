//! The locale natives of `string.c` under the one locale this engine
//! has, C's: `setlocale`, `localeconv`, `nl_langinfo` and `strcoll`.
//! The request keeps each category's name as the host's `setlocale(3)`
//! reports it, starting where php leaves it after
//! `zend_reset_lc_ctype_locale`, `C.UTF-8` for `LC_CTYPE` and `C` for
//! the rest, and dropped at the request's end as `basic_functions`'s
//! `RSHUTDOWN` sets it back. The names a request can move a category
//! to are the C locale's own, `C`, `POSIX` and `C.UTF-8`; what php
//! answers for every other name is the host's locale table, and
//! passing a name through to it is #640's.

use php_ext::abi::{OrFalse, Untyped, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::to_string;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};

use super::constants::{
    self, CHAR_MAX, CODESET, LC_ALL, LC_COLLATE, LC_CTYPE, LC_MESSAGES, LC_MONETARY, LC_NUMERIC,
    LC_TIME,
};

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

/// `nl_langinfo`'s items under the C locale, indexed by the pinned
/// build's `<langinfo.h>` values, `CODESET` (0) aside since it follows
/// `LC_CTYPE`.
const LANGINFO: [(i64, &[u8]); 57] = [
    (constants::CODESET, b""),
    (constants::D_T_FMT, b"%a %b %e %H:%M:%S %Y"),
    (constants::D_FMT, b"%m/%d/%y"),
    (constants::T_FMT, b"%H:%M:%S"),
    (constants::T_FMT_AMPM, b"%I:%M:%S %p"),
    (constants::AM_STR, b"AM"),
    (constants::PM_STR, b"PM"),
    (constants::DAY_1, b"Sunday"),
    (constants::DAY_2, b"Monday"),
    (constants::DAY_3, b"Tuesday"),
    (constants::DAY_4, b"Wednesday"),
    (constants::DAY_5, b"Thursday"),
    (constants::DAY_6, b"Friday"),
    (constants::DAY_7, b"Saturday"),
    (constants::ABDAY_1, b"Sun"),
    (constants::ABDAY_2, b"Mon"),
    (constants::ABDAY_3, b"Tue"),
    (constants::ABDAY_4, b"Wed"),
    (constants::ABDAY_5, b"Thu"),
    (constants::ABDAY_6, b"Fri"),
    (constants::ABDAY_7, b"Sat"),
    (constants::MON_1, b"January"),
    (constants::MON_2, b"February"),
    (constants::MON_3, b"March"),
    (constants::MON_4, b"April"),
    (constants::MON_5, b"May"),
    (constants::MON_6, b"June"),
    (constants::MON_7, b"July"),
    (constants::MON_8, b"August"),
    (constants::MON_9, b"September"),
    (constants::MON_10, b"October"),
    (constants::MON_11, b"November"),
    (constants::MON_12, b"December"),
    (constants::ABMON_1, b"Jan"),
    (constants::ABMON_2, b"Feb"),
    (constants::ABMON_3, b"Mar"),
    (constants::ABMON_4, b"Apr"),
    (constants::ABMON_5, b"May"),
    (constants::ABMON_6, b"Jun"),
    (constants::ABMON_7, b"Jul"),
    (constants::ABMON_8, b"Aug"),
    (constants::ABMON_9, b"Sep"),
    (constants::ABMON_10, b"Oct"),
    (constants::ABMON_11, b"Nov"),
    (constants::ABMON_12, b"Dec"),
    (constants::ERA, b""),
    (constants::ERA_D_FMT, b""),
    (constants::ERA_D_T_FMT, b""),
    (constants::ERA_T_FMT, b""),
    (constants::ALT_DIGITS, b""),
    (constants::RADIXCHAR, b"."),
    (constants::THOUSEP, b""),
    (constants::YESEXPR, b"^[yY]"),
    (constants::NOEXPR, b"^[nN]"),
    (constants::YESSTR, b"yes"),
    (constants::NOSTR, b"no"),
    (constants::CRNCYSTR, b""),
];

/// Each category's current name, by `<locale.h>` value, `LC_ALL`'s
/// slot unused.
pub(super) struct Locale {
    names: [Vec<u8>; 7],
}

impl Default for Locale {
    fn default() -> Locale {
        let mut names: [Vec<u8>; 7] = std::array::from_fn(|_| b"C".to_vec());
        names[2] = b"C.UTF-8".to_vec();
        Locale { names }
    }
}

impl Locale {
    /// `setlocale(LC_ALL, NULL)`: the one name when every category
    /// agrees, else the six in order joined by `/`.
    fn all(&self) -> Vec<u8> {
        let names = &self.names[1..];
        if names.iter().all(|n| *n == names[0]) {
            return names[0].clone();
        }
        names.join(&b'/')
    }

    /// `setlocale(category, NULL)`.
    fn query(&self, category: usize) -> Vec<u8> {
        if category == 0 {
            self.all()
        } else {
            self.names[category].clone()
        }
    }
}

/// A name the C library's C locale answers to.
fn c_locale(name: &[u8]) -> bool {
    matches!(name, b"C" | b"POSIX" | b"C.UTF-8")
}

/// The name `setlocale(category, "")` reads for one category off the
/// request's environment: `LC_ALL`, then the category's variable, then
/// `LANG`, then `C`.
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

/// `try_setlocale_str(cat, loc)`: the name answered, or `None` where
/// the C library refuses.
fn try_setlocale(req: &mut Request, category: i64, loc: &[u8]) -> Option<Vec<u8>> {
    let category = usize::try_from(category).ok().filter(|c| *c < 7)?;
    if loc == b"0" {
        return Some(req.state::<crate::State>().string.locale.query(category));
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
    let mut names = Vec::with_capacity(targets.len());
    for target in &targets {
        let name = if loc.is_empty() {
            from_environment(req, *target)
        } else {
            loc.to_vec()
        };
        if !c_locale(&name) {
            return None;
        }
        names.push(name);
    }
    let locale = &mut req.state::<crate::State>().string.locale;
    for (target, name) in targets.iter().zip(names) {
        locale.names[*target] = name;
    }
    let retval = locale.query(category);
    if (category == 0 || category == 2) && retval == b"C" {
        return Some(b"C".to_vec());
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
#[php_function]
pub(super) fn setlocale(
    req: &mut Request,
    category: i64,
    locales: Untyped<Value>,
    rest: Variadic<Untyped<Value>>,
) -> Result<OrFalse<PhpString>, Thrown> {
    let given: Vec<Value> = std::iter::once(locales.0)
        .chain(rest.0.into_iter().map(|rest| rest.0))
        .collect();
    let mut candidates: Vec<Value> = Vec::with_capacity(given.len());
    for (value, position) in given.into_iter().zip(2u32..) {
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
                for (_, element) in array.snapshot() {
                    names.push(to_string(req, &element.deref())?.as_bytes().to_vec());
                }
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

/// `localeconv(): array`: the C locale's `struct lconv`, `CHAR_MAX` in
/// every numeric field and both groupings empty.
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
#[php_function]
pub(super) fn localeconv(req: &mut Request) -> Result<PhpArray, Thrown> {
    let mut array = PhpArray::new(req)?;
    let strings: [(&[u8], &[u8]); 8] = [
        (b"decimal_point", b"."),
        (b"thousands_sep", b""),
        (b"int_curr_symbol", b""),
        (b"currency_symbol", b""),
        (b"mon_decimal_point", b""),
        (b"mon_thousands_sep", b""),
        (b"positive_sign", b""),
        (b"negative_sign", b""),
    ];
    for (name, value) in strings {
        let key = Key::Str(PhpString::new(req, name)?);
        let value = Value::string(PhpString::new(req, value)?);
        array.set(req, key, value)?;
    }
    for name in [
        &b"int_frac_digits"[..],
        b"frac_digits",
        b"p_cs_precedes",
        b"p_sep_by_space",
        b"n_cs_precedes",
        b"n_sep_by_space",
        b"p_sign_posn",
        b"n_sign_posn",
    ] {
        let key = Key::Str(PhpString::new(req, name)?);
        array.set(req, key, Value::int(CHAR_MAX))?;
    }
    for name in [&b"grouping"[..], b"mon_grouping"] {
        let key = Key::Str(PhpString::new(req, name)?);
        let empty = PhpArray::new(req)?;
        array.set(req, key, Value::array(empty))?;
    }
    Ok(array)
}

/// `nl_langinfo(int $item): string|false`: the C locale's answer for
/// one of the items the build defines, `CODESET` read off `LC_CTYPE`,
/// and the warning and `false` for any other number.
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
#[php_function]
pub(super) fn nl_langinfo(req: &mut Request, item: i64) -> Result<OrFalse<PhpString>, Thrown> {
    let Some((_, answer)) = LANGINFO.iter().find(|(id, _)| *id == item) else {
        docref(req, Level::Warning, &format!("Item '{item}' is not valid"));
        return Ok(OrFalse::False);
    };
    if item == CODESET {
        let utf8 = req.state::<crate::State>().string.locale.names[2] == b"C.UTF-8";
        return PhpString::new(req, if utf8 { b"UTF-8" } else { b"US-ASCII" }).map(OrFalse::Value);
    }
    PhpString::new(req, answer).map(OrFalse::Value)
}

/// `strcoll(string $string1, string $string2): int`: the C locale's
/// collation, `strcmp` over the bytes up to each string's first NUL,
/// whose answer on the pinned build is the difference of the first
/// bytes that differ.
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
#[php_function]
pub(super) fn strcoll(string1: PhpString, string2: PhpString) -> i64 {
    let (s1, s2) = (string1, string2);
    let c_str = |s: &[u8]| -> Vec<u8> { s.split(|c| *c == 0).next().unwrap_or(&[]).to_vec() };
    let (left, right) = (c_str(s1.as_bytes()), c_str(s2.as_bytes()));
    let mut at = 0;
    loop {
        let x = left.get(at).copied().unwrap_or(0);
        let y = right.get(at).copied().unwrap_or(0);
        if x != y || x == 0 {
            return i64::from(x) - i64::from(y);
        }
        at += 1;
    }
}
