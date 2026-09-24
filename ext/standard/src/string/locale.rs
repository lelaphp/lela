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

use php_ext::coerce::to_string;
use php_ext::decl::ConstDecl;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};

use crate::zpp::{long_of, string_value};
use crate::{argument_count_between, constant, long_argument, string_argument};

/// `LC_ALL` and the six categories, the values of the pinned build's
/// `<locale.h>`, in `setlocale(LC_ALL, NULL)`'s composite order.
const LC_ALL: i64 = 0;
const CATEGORIES: [(&[u8], i64); 7] = [
    (b"LC_CTYPE", 2),
    (b"LC_NUMERIC", 4),
    (b"LC_TIME", 5),
    (b"LC_COLLATE", 1),
    (b"LC_MONETARY", 3),
    (b"LC_ALL", LC_ALL),
    (b"LC_MESSAGES", 6),
];

/// `nl_langinfo`'s items under the C locale, indexed by the pinned
/// build's `<langinfo.h>` values, `CODESET` (0) aside since it follows
/// `LC_CTYPE`.
const LANGINFO: [(&[u8], &[u8]); 57] = [
    (b"CODESET", b""),
    (b"D_T_FMT", b"%a %b %e %H:%M:%S %Y"),
    (b"D_FMT", b"%m/%d/%y"),
    (b"T_FMT", b"%H:%M:%S"),
    (b"T_FMT_AMPM", b"%I:%M:%S %p"),
    (b"AM_STR", b"AM"),
    (b"PM_STR", b"PM"),
    (b"DAY_1", b"Sunday"),
    (b"DAY_2", b"Monday"),
    (b"DAY_3", b"Tuesday"),
    (b"DAY_4", b"Wednesday"),
    (b"DAY_5", b"Thursday"),
    (b"DAY_6", b"Friday"),
    (b"DAY_7", b"Saturday"),
    (b"ABDAY_1", b"Sun"),
    (b"ABDAY_2", b"Mon"),
    (b"ABDAY_3", b"Tue"),
    (b"ABDAY_4", b"Wed"),
    (b"ABDAY_5", b"Thu"),
    (b"ABDAY_6", b"Fri"),
    (b"ABDAY_7", b"Sat"),
    (b"MON_1", b"January"),
    (b"MON_2", b"February"),
    (b"MON_3", b"March"),
    (b"MON_4", b"April"),
    (b"MON_5", b"May"),
    (b"MON_6", b"June"),
    (b"MON_7", b"July"),
    (b"MON_8", b"August"),
    (b"MON_9", b"September"),
    (b"MON_10", b"October"),
    (b"MON_11", b"November"),
    (b"MON_12", b"December"),
    (b"ABMON_1", b"Jan"),
    (b"ABMON_2", b"Feb"),
    (b"ABMON_3", b"Mar"),
    (b"ABMON_4", b"Apr"),
    (b"ABMON_5", b"May"),
    (b"ABMON_6", b"Jun"),
    (b"ABMON_7", b"Jul"),
    (b"ABMON_8", b"Aug"),
    (b"ABMON_9", b"Sep"),
    (b"ABMON_10", b"Oct"),
    (b"ABMON_11", b"Nov"),
    (b"ABMON_12", b"Dec"),
    (b"ERA", b""),
    (b"ERA_D_FMT", b""),
    (b"ERA_D_T_FMT", b""),
    (b"ERA_T_FMT", b""),
    (b"ALT_DIGITS", b""),
    (b"RADIXCHAR", b"."),
    (b"THOUSEP", b""),
    (b"YESEXPR", b"^[yY]"),
    (b"NOEXPR", b"^[nN]"),
    (b"YESSTR", b"yes"),
    (b"NOSTR", b"no"),
    (b"CRNCYSTR", b""),
];

/// The constants of `string.c`'s locale natives: `CHAR_MAX`, the
/// `LC_*` categories and the `nl_langinfo` items.
pub(super) fn constants() -> Vec<ConstDecl> {
    let mut out = vec![constant(b"CHAR_MAX", 127)];
    for (name, value) in CATEGORIES {
        out.push(constant(name, value));
    }
    for (value, (name, _)) in LANGINFO.iter().enumerate() {
        out.push(constant(name, i64::try_from(value).unwrap_or(0)));
    }
    out
}

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
pub(super) fn setlocale(req: &mut Request) -> Result<Value, Thrown> {
    let given = req.argument_count();
    if given < 2 {
        return Err(php_ext::raising::too_few_arguments(
            req,
            2,
            u32::try_from(given).unwrap_or(0),
        ));
    }
    let category = long_argument(req, 1)?;
    let mut candidates: Vec<Value> = Vec::with_capacity(given - 1);
    for position in 2..=given {
        let position = u32::try_from(position).unwrap_or(u32::MAX);
        let value = req.argument(position).unwrap_or_default();
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
                return string_value(req, &answer);
            }
        }
    }
    Ok(Value::bool(false))
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
pub(super) fn localeconv(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
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
        let value = string_value(req, value)?;
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
        array.set(req, key, long_of(127))?;
    }
    for name in [&b"grouping"[..], b"mon_grouping"] {
        let key = Key::Str(PhpString::new(req, name)?);
        let empty = PhpArray::new(req)?;
        array.set(req, key, Value::array(empty))?;
    }
    Ok(Value::array(array))
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
pub(super) fn nl_langinfo(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let item = long_argument(req, 1)?;
    let Some((_, answer)) = usize::try_from(item).ok().and_then(|i| LANGINFO.get(i)) else {
        docref(req, Level::Warning, &format!("Item '{item}' is not valid"));
        return Ok(Value::bool(false));
    };
    if item == 0 {
        let utf8 = req.state::<crate::State>().string.locale.names[2] == b"C.UTF-8";
        return string_value(req, if utf8 { b"UTF-8" } else { b"US-ASCII" });
    }
    string_value(req, answer)
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
pub(super) fn strcoll(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let s1 = string_argument(req, 1)?;
    let s2 = string_argument(req, 2)?;
    let c_str = |s: &[u8]| -> Vec<u8> { s.split(|c| *c == 0).next().unwrap_or(&[]).to_vec() };
    let (left, right) = (c_str(s1.as_bytes()), c_str(s2.as_bytes()));
    let mut at = 0;
    loop {
        let x = left.get(at).copied().unwrap_or(0);
        let y = right.get(at).copied().unwrap_or(0);
        if x != y || x == 0 {
            return Ok(Value::int(i64::from(x) - i64::from(y)));
        }
        at += 1;
    }
}
