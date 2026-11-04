//! `locale/`: the `Locale` class and the `locale_*` functions, its
//! static methods php aliases to them, over ICU's `uloc_*` C API as
//! `locale_methods.cpp` calls it: the grandfathered tags and the
//! singleton walk are php's, the parts of an ID ICU's.

use php_ext::abi::{ExtensionBuilder, OrFalse, Path, True};
use php_ext::attr::{php_class, php_function};
use php_ext::decl::{ClassConstDecl, ConstExpr, Name, Type, Visibility};
use php_ext::raising::{Thrown, arg_type_error, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value, walk};

use crate::error;
use crate::ffi::locale::{self as uloc, Part, Shown};
use crate::ffi::{self, Code, c_string};

/// `INTL_MAX_LOCALE_LEN`.
const MAX_LOCALE_LEN: usize = 156;

/// `LOC_GRANDFATHERED` beside `LOC_PREFERRED_GRANDFATHERED`: the IANA
/// language subtag list's grandfathered and redundant tags and their preferred
/// values (File-Date: 2021-08-06).
const GRANDFATHERED: [(&str, Option<&str>); 50] = [
    ("art-lojban", Some("jbo")),
    ("cel-gaulish", None),
    ("en-GB-oed", Some("en-GB-oxendict")),
    ("i-ami", Some("ami")),
    ("i-bnn", Some("bnn")),
    ("i-default", None),
    ("i-enochian", None),
    ("i-hak", Some("hak")),
    ("i-klingon", Some("tlh")),
    ("i-lux", Some("lb")),
    ("i-mingo", None),
    ("i-navajo", Some("nv")),
    ("i-pwn", Some("pwn")),
    ("i-tao", Some("tao")),
    ("i-tay", Some("tay")),
    ("i-tsu", Some("tsu")),
    ("no-bok", Some("nb")),
    ("no-nyn", Some("nn")),
    ("sgn-BE-FR", Some("sfb")),
    ("sgn-BE-NL", Some("vgt")),
    ("sgn-BR", Some("bzs")),
    ("sgn-CH-DE", Some("sgg")),
    ("sgn-CO", Some("csn")),
    ("sgn-DE", Some("gsg")),
    ("sgn-DK", Some("dsl")),
    ("sgn-ES", Some("ssp")),
    ("sgn-FR", Some("fsl")),
    ("sgn-GB", Some("bfi")),
    ("sgn-GR", Some("gss")),
    ("sgn-IE", Some("isg")),
    ("sgn-IT", Some("ise")),
    ("sgn-JP", Some("jsl")),
    ("sgn-MX", Some("mfs")),
    ("sgn-NI", Some("ncs")),
    ("sgn-NL", Some("dse")),
    ("sgn-NO", Some("nsl")),
    ("sgn-PT", Some("psr")),
    ("sgn-SE", Some("swl")),
    ("sgn-US", Some("ase")),
    ("sgn-ZA", Some("sfs")),
    ("zh-cmn", Some("cmn")),
    ("zh-cmn-Hans", Some("cmn-Hans")),
    ("zh-cmn-Hant", Some("cmn-Hant")),
    ("zh-gan", Some("gan")),
    ("zh-guoyu", Some("cmn")),
    ("zh-hakka", Some("hak")),
    ("zh-min", None),
    ("zh-min-nan", Some("nan")),
    ("zh-wuu", Some("wuu")),
    ("zh-xiang", Some("hsn")),
];

/// The tags php names a locale's parts by, `LOC_*_TAG`.
const LANG: &str = "language";
const SCRIPT: &str = "script";
const REGION: &str = "region";
const VARIANT: &str = "variant";
const EXTLANG: &str = "extlang";
const GRANDFATHERED_TAG: &str = "grandfathered";
const PRIVATE: &str = "private";

/// `findOffset(LOC_GRANDFATHERED, ...)`, case-blind.
fn grandfathered(locale: &[u8]) -> Option<usize> {
    GRANDFATHERED
        .iter()
        .position(|(tag, _)| tag.as_bytes().eq_ignore_ascii_case(locale))
}

/// `isIDSeparator`.
fn is_separator(c: Option<&u8>) -> bool {
    matches!(c, Some(b'_' | b'-'))
}

/// `getSingletonPos`: where a one-letter subtag stands, `Some(0)` for
/// one leading the ID.
fn singleton_pos(s: &[u8]) -> Option<usize> {
    for i in 0..s.len() {
        if is_separator(s.get(i)) {
            if i == 1 {
                return Some(0);
            }
            if is_separator(s.get(i + 2)) {
                return Some(i + 1);
            }
        }
    }
    None
}

/// `getStrrtokenPos`: the end of the next shorter prefix lookup tries,
/// a singleton dropped with its subtag.
fn strrtoken_pos(s: &[u8], saved: usize) -> usize {
    for i in (0..saved).rev() {
        if is_separator(s.get(i)) || s[i] == b'@' {
            let pos = if i >= 2 && is_separator(s.get(i - 2)) {
                i - 2
            } else {
                i
            };
            return if pos < 1 { 0 } else { pos };
        }
    }
    0
}

/// `strToMatch`: lower case, `-` as `_`; `None` for the empty string.
fn to_match(s: &[u8]) -> Option<Vec<u8>> {
    (!s.is_empty()).then(|| {
        s.iter()
            .map(|&c| {
                if c == b'-' {
                    b'_'
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect()
    })
}

/// What `get_icu_value_internal` answers: a value, none (`result == -1`)
/// or a failure (`NULL` with `result` left at 0).
enum Tagged {
    Value(Vec<u8>),
    Empty,
    Failed,
}

/// The part of an ID a tag names.
fn part_of(tag: &str) -> Part {
    match tag {
        LANG => Part::Language,
        SCRIPT => Part::Script,
        REGION => Part::Country,
        VARIANT => Part::Variant,
        _ => Part::Canonical,
    }
}

/// `get_icu_value_internal`: a grandfathered tag is its own language
/// and nothing else, and from `locale_parse` an `x-`/`i-` prefix and a
/// leading singleton end the walk, a later singleton cutting the ID.
fn icu_value(locale: &[u8], tag: Option<&str>, from_parse: bool) -> Tagged {
    if locale.len() > MAX_LOCALE_LEN {
        return Tagged::Failed;
    }
    let mut name = locale;
    if let Some(tag) = tag {
        if grandfathered(locale).is_some() {
            return if tag == LANG {
                Tagged::Value(locale.to_vec())
            } else {
                Tagged::Failed
            };
        }
        if from_parse {
            if tag == LANG
                && locale.len() > 1
                && matches!(locale[0], b'x' | b'X' | b'i' | b'I')
                && is_separator(locale.get(1))
            {
                return Tagged::Failed;
            }
            match singleton_pos(locale) {
                Some(0) => return Tagged::Failed,
                Some(at) => name = &locale[..at - 1],
                None => {}
            }
        }
    }
    let (value, status) = uloc::part(tag.map_or(Part::Canonical, part_of), &c_string(name));
    if status.is_failure() {
        return Tagged::Failed;
    }
    let end = value.iter().position(|&c| c == 0).unwrap_or(value.len());
    if end == 0 {
        Tagged::Empty
    } else {
        Tagged::Value(value[..end].to_vec())
    }
}

/// The locale a native was handed, `intl.default_locale` for the empty
/// string.
fn or_default(req: &Request, locale: &[u8]) -> Vec<u8> {
    if locale.is_empty() {
        crate::default_locale(req)
    } else {
        locale.to_vec()
    }
}

/// `INTL_CHECK_LOCALE_LEN`: the error set and `Err(None)` for an ID
/// longer than `INTL_MAX_LOCALE_LEN`.
fn check_len(req: &mut Request, len: usize) -> Result<(), Option<Thrown>> {
    if len > MAX_LOCALE_LEN {
        let text =
            format!("Locale string too long, should be no longer than {MAX_LOCALE_LEN} characters");
        return Err(error::set(req, Code::ILLEGAL_ARGUMENT, &text).err());
    }
    Ok(())
}

/// A native's answer where `Err(None)` is php's `RETURN_NULL()`.
fn or_null<T>(answer: Result<T, Option<Thrown>>) -> Result<Option<T>, Thrown> {
    match answer {
        Ok(value) => Ok(Some(value)),
        Err(None) => Ok(None),
        Err(Some(thrown)) => Err(thrown),
    }
}

/// A native's answer where `Err(None)` is php's `RETURN_FALSE`.
fn or_false<T>(answer: Result<T, Option<Thrown>>) -> Result<OrFalse<T>, Thrown> {
    or_null(answer).map(|value| value.map_or(OrFalse::False, OrFalse::Value))
}

/// `get_icu_value_src_php`: one part of the locale, or its canonical
/// form, the empty string where it has none.
fn get_value(
    req: &mut Request,
    locale: Path,
    tag: Option<&str>,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let locale = or_default(req, locale.0.as_bytes());
    or_null(check_len(req, locale.len()))?.map_or(Ok(None), |()| {
        match icu_value(&locale, tag, false) {
            Tagged::Value(value) => PhpString::new(req, &value).map(Some),
            Tagged::Empty => PhpString::new(req, b"").map(Some),
            Tagged::Failed => {
                let text = format!("unable to get locale {}", tag.unwrap_or("canonicalize"));
                error::set(req, Code::ZERO, &text)?;
                Ok(None)
            }
        }
    })
}

/// `get_icu_disp_value_src_php`: a part's name, or the whole ID's, in
/// `display` or the default locale's language.
fn get_display(
    req: &mut Request,
    locale: Path,
    display: Option<Path>,
    shown: Shown,
    tag: &str,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let locale = locale.0.as_bytes();
    if locale.len() > ffi::FULLNAME_CAPACITY {
        error::set(req, Code::ILLEGAL_ARGUMENT, "name too long")?;
        return Ok(OrFalse::False);
    }
    let mut locale = or_default(req, locale);
    if tag != "name"
        && let Some(at) = grandfathered(&locale)
    {
        if tag != LANG {
            return Ok(OrFalse::False);
        }
        let (tag, preferred) = GRANDFATHERED[at];
        locale = preferred.unwrap_or(tag).as_bytes().to_vec();
    }
    let display = match display {
        Some(display) => display.0.as_bytes().to_vec(),
        None => crate::default_locale(req),
    };
    let (name, status) = uloc::display(shown, &c_string(&locale), &c_string(&display));
    if status.is_failure() {
        error::set(req, status, format!("unable to get locale {tag}"))?;
        return Ok(OrFalse::False);
    }
    match ffi::to_utf8(&name) {
        Ok(text) => PhpString::new(req, &text).map(OrFalse::Value),
        Err(status) => {
            let text = format!("error converting display name for {tag} to UTF-8");
            error::set(req, status, &text)?;
            Ok(OrFalse::False)
        }
    }
}

/// A string value for an array.
fn text(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    PhpString::new(req, bytes).map(Owner::into_value)
}

/// `add_assoc_*`: `key => value` set on the array.
fn assoc(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    key: &[u8],
    value: &[u8],
) -> Result<(), Thrown> {
    let value = text(req, value)?;
    array.set_str(req, key, value)
}

/// `locale_get_keywords`: the keywords as `name => value`, `null` where
/// the ID has none.
fn get_keywords(
    req: &mut Request,
    locale: Path,
) -> Result<Option<OrFalse<Owner<PhpArray>>>, Thrown> {
    let locale = locale.0.as_bytes();
    if or_null(check_len(req, locale.len()))?.is_none() {
        return Ok(None);
    }
    let locale = c_string(&or_default(req, locale));
    let mut status = Code::ZERO;
    let Some(mut keywords) = uloc::keywords(&locale, &mut status) else {
        return Ok(None);
    };
    let mut array = PhpArray::new(req)?;
    while let Some(key) = keywords.next(&mut status) {
        let (value, code) = uloc::keyword_value(&locale, &c_string(&key));
        if code.is_failure() {
            array.give_up(req);
            error::set(
                req,
                Code::ILLEGAL_ARGUMENT,
                "Error encountered while getting the keyword value for the keyword",
            )?;
            return Ok(Some(OrFalse::False));
        }
        assoc(req, &mut array, &key, &value)?;
    }
    Ok(Some(OrFalse::Value(array)))
}

/// What `append_key_value` and its siblings answer: php's `SUCCESS`,
/// `FAILURE` and `LOC_NOT_FOUND`.
#[derive(PartialEq, Eq)]
enum Appended {
    Done,
    NotString,
    NotFound,
}

/// `append_key_value`: one subtag by its key, `_` before all but the
/// language.
fn append_key_value(out: &mut Vec<u8>, array: PhpArray, key: &str) -> Appended {
    let Some(value) = array.find(key.as_bytes()).map(|v| v.deref()) else {
        return Appended::NotFound;
    };
    let Some(bytes) = value.as_str_bytes() else {
        return Appended::NotString;
    };
    if key != LANG && key != GRANDFATHERED_TAG {
        out.push(b'_');
    }
    out.extend_from_slice(bytes);
    Appended::Done
}

/// `append_multiple_key_values`: a variant, extlang or private subtag
/// list, as a string, an array, or `key0`, `key1`, ...
fn append_multiple(req: &Request, out: &mut Vec<u8>, array: PhpArray, key: &str) -> Appended {
    let prefix = |out: &mut Vec<u8>| {
        if key == PRIVATE {
            out.extend_from_slice(b"_x");
        }
    };
    let mut first = true;
    let mut add = |out: &mut Vec<u8>, value: Value| {
        let Some(bytes) = value.deref().as_str_bytes().map(<[u8]>::to_vec) else {
            return false;
        };
        if std::mem::take(&mut first) {
            prefix(out);
        }
        out.push(b'_');
        out.extend_from_slice(&bytes);
        true
    };
    if let Some(value) = array.find(key.as_bytes()).map(|v| v.deref()) {
        if let Some(list) = value.as_array() {
            for (_, element) in walk::rows(req, list) {
                if !add(out, element) {
                    return Appended::NotString;
                }
            }
            return Appended::Done;
        }
        return if add(out, value) {
            Appended::Done
        } else {
            Appended::NotString
        };
    }
    let max = match key {
        VARIANT | PRIVATE => 15,
        _ => 3,
    };
    for i in 0..max {
        if let Some(value) = array.find(format!("{key}{i}").as_bytes())
            && !add(out, value)
        {
            return Appended::NotString;
        }
    }
    Appended::Done
}

/// `locale_compose`: an ID from its subtags.
fn compose(req: &mut Request, subtags: PhpArray) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    if subtags.is_empty() {
        return Ok(OrFalse::False);
    }
    let mut out = Vec::new();
    // `handleAppendResult`: the error reset, and set where a subtag is
    // not a string.
    let step = |req: &mut Request, appended: Appended| -> Result<bool, Thrown> {
        error::reset(req);
        if appended == Appended::NotString {
            error::set(
                req,
                Code::ILLEGAL_ARGUMENT,
                "parameter array element is not a string",
            )?;
            return Ok(false);
        }
        Ok(true)
    };
    let appended = append_key_value(&mut out, subtags, GRANDFATHERED_TAG);
    if appended == Appended::Done {
        return PhpString::new(req, &out).map(OrFalse::Value);
    }
    if !step(req, appended)? {
        return Ok(OrFalse::False);
    }
    let appended = append_key_value(&mut out, subtags, LANG);
    if appended == Appended::NotFound {
        return Err(arg_value_error(req, 1, "contain a \"language\" key"));
    }
    if !step(req, appended)? {
        return Ok(OrFalse::False);
    }
    for (key, multiple) in [
        (EXTLANG, true),
        (SCRIPT, false),
        (REGION, false),
        (VARIANT, true),
        (PRIVATE, true),
    ] {
        let appended = if multiple {
            append_multiple(req, &mut out, subtags, key)
        } else {
            append_key_value(&mut out, subtags, key)
        };
        if !step(req, appended)? {
            return Ok(OrFalse::False);
        }
    }
    PhpString::new(req, &out).map(OrFalse::Value)
}

/// `get_private_subtags`: what follows the `x` singleton.
fn private_subtags(locale: &[u8]) -> Option<&[u8]> {
    let mut rest = locale;
    while let Some(at) = singleton_pos(rest) {
        if matches!(rest[at], b'x' | b'X') {
            return (at + 2 != rest.len()).then(|| rest.get(at + 2..).unwrap_or_default());
        }
        if at + 1 >= rest.len() {
            break;
        }
        rest = &rest[at + 1..];
    }
    None
}

/// `php_strtok_r` over `-_`: the tokens, the first whatever its
/// length, then each of two letters or more up to the first shorter.
fn tokens(value: &[u8]) -> Vec<&[u8]> {
    let mut all = value
        .split(|&c| c == b'-' || c == b'_')
        .filter(|token| !token.is_empty());
    let mut out: Vec<&[u8]> = all.next().into_iter().collect();
    out.extend(all.take_while(|token| token.len() > 1));
    out
}

/// `add_array_entry`: one part of a parsed ID, a list of them for the
/// variants and the private subtags.
fn add_entry(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    locale: &[u8],
    tag: &str,
) -> Result<(), Thrown> {
    let value = if tag == PRIVATE {
        private_subtags(locale).map(<[u8]>::to_vec)
    } else {
        match icu_value(locale, Some(tag), true) {
            Tagged::Value(value) => Some(value),
            _ => None,
        }
    };
    let Some(value) = value else {
        return Ok(());
    };
    if tag == PRIVATE || tag == VARIANT {
        for (i, token) in tokens(&value).into_iter().enumerate() {
            assoc(req, array, format!("{tag}{i}").as_bytes(), token)?;
        }
        return Ok(());
    }
    assoc(req, array, tag.as_bytes(), &value)
}

/// `locale_parse`: an ID's subtags by name.
fn parse(req: &mut Request, locale: Path) -> Result<Option<Owner<PhpArray>>, Thrown> {
    let locale = locale.0.as_bytes();
    if or_null(check_len(req, locale.len()))?.is_none() {
        return Ok(None);
    }
    let locale = or_default(req, locale);
    let mut array = PhpArray::new(req)?;
    if grandfathered(&locale).is_some() {
        assoc(req, &mut array, GRANDFATHERED_TAG.as_bytes(), &locale)?;
    } else {
        for tag in [LANG, SCRIPT, REGION, VARIANT, PRIVATE] {
            add_entry(req, &mut array, &locale, tag)?;
        }
    }
    Ok(Some(array))
}

/// `locale_get_all_variants`: the variant subtags as a list.
fn all_variants(req: &mut Request, locale: Path) -> Result<Option<Owner<PhpArray>>, Thrown> {
    let locale = or_default(req, locale.0.as_bytes());
    if or_null(check_len(req, locale.len()))?.is_none() {
        return Ok(None);
    }
    let mut array = PhpArray::new(req)?;
    if grandfathered(&locale).is_none()
        && let Tagged::Value(variant) = icu_value(&locale, Some(VARIANT), false)
    {
        for token in tokens(&variant) {
            let value = text(req, token)?;
            array.append(req, value)?;
        }
    }
    Ok(Some(array))
}

/// `locale_filter_matches`: RFC 4647's basic filtering, the range a
/// prefix of the tag up to a separator.
fn filter_matches(
    req: &mut Request,
    tag: Path,
    range: Path,
    canonicalize: bool,
) -> Result<Option<bool>, Thrown> {
    let range = or_default(req, range.0.as_bytes());
    if range == b"*" {
        return Ok(Some(true));
    }
    let tag = tag.0.as_bytes();
    if or_null(check_len(req, range.len()))?.is_none()
        || or_null(check_len(req, tag.len()))?.is_none()
    {
        return Ok(None);
    }
    let (tag, range) = if canonicalize {
        let Tagged::Value(range) = icu_value(&range, None, false) else {
            error::set(req, Code::ZERO, "unable to canonicalize loc_range")?;
            return Ok(Some(false));
        };
        let Tagged::Value(tag) = icu_value(tag, None, false) else {
            error::set(req, Code::ZERO, "unable to canonicalize lang_tag")?;
            return Ok(Some(false));
        };
        (tag, range)
    } else {
        (tag.to_vec(), range)
    };
    let (Some(tag), Some(range)) = (to_match(&tag), to_match(&range)) else {
        return Ok(Some(false));
    };
    let after = tag.get(range.len());
    Ok(Some(
        tag.starts_with(&range)
            && (after.is_none() || is_separator(after) || (canonicalize && after == Some(&b'@'))),
    ))
}

/// `lookup_loc_range`: the tag that best matches the range, trying
/// shorter prefixes of the range in turn; `Ok(None)` where none does or
/// the error is set.
fn lookup_range(
    req: &mut Request,
    range: &[u8],
    tags: PhpArray,
    canonicalize: bool,
) -> Result<Option<Vec<u8>>, Thrown> {
    let mut list: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let fail = |req: &mut Request, what: &str| {
        let text = format!("unable to canonicalize {what}");
        error::set(req, Code::ILLEGAL_ARGUMENT, &text).map(|()| None)
    };
    // Read under the rows walk, each row's bytes copied, and refused
    // after it, where a diagnostic can reach the program.
    let read: Vec<Option<Vec<u8>>> = walk::rows(req, tags)
        .map(|(_, value)| value.deref().as_str_bytes().map(<[u8]>::to_vec))
        .collect();
    for original in read {
        let Some(original) = original else {
            return Err(arg_type_error(req, 2, "only contain string values"));
        };
        if original.contains(&0) {
            return Err(arg_value_error(req, 2, "not contain any null bytes"));
        }
        let Some(lowered) = to_match(&original) else {
            return fail(req, "lang_tag");
        };
        list.push((lowered, original));
    }
    if canonicalize {
        for (lowered, original) in &mut list {
            let Tagged::Value(canonical) = icu_value(original, None, false) else {
                return fail(req, "lang_tag");
            };
            let Some(canonical) = to_match(&canonical) else {
                return fail(req, "lang_tag");
            };
            *lowered = canonical;
        }
    }
    let range = if canonicalize {
        match icu_value(range, None, false) {
            Tagged::Value(range) => range,
            _ => return fail(req, "loc_range"),
        }
    } else {
        range.to_vec()
    };
    let Some(range) = to_match(&range) else {
        return fail(req, "lang_tag");
    };
    let mut saved = range.len();
    while saved > 0 {
        for (lowered, original) in &list {
            if lowered.len() == saved && range[..saved] == lowered[..] {
                return Ok(Some(if canonicalize { lowered } else { original }.clone()));
            }
        }
        saved = strrtoken_pos(&range, saved);
    }
    Ok(None)
}

/// `locale_lookup`: the best match for the range among the tags, the
/// fallback where none matches.
fn lookup(
    req: &mut Request,
    tags: PhpArray,
    range: Path,
    canonicalize: bool,
    fallback: Option<Path>,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let range = match (range.0.as_bytes(), fallback) {
        (b"", Some(fallback)) => fallback.0.as_bytes().to_vec(),
        (range, _) => or_default(req, range),
    };
    if or_null(check_len(req, range.len()))?.is_none() {
        return Ok(None);
    }
    if tags.is_empty() {
        return PhpString::new(req, b"").map(Some);
    }
    let found = lookup_range(req, &range, tags, canonicalize)?;
    if error::code(req).is_failure() {
        return Ok(None);
    }
    match found {
        Some(found) if !found.is_empty() => PhpString::new(req, &found).map(Some),
        _ => match fallback {
            Some(fallback) => Ok(Some(Owner::take(req, fallback.0))),
            None => PhpString::new(req, b"").map(Some),
        },
    }
}

/// `locale_accept_from_http`: the available locale an `Accept-Language`
/// header prefers.
fn accept_from_http(
    req: &mut Request,
    header: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let header = header.as_bytes();
    if header.len() > ffi::FULLNAME_CAPACITY
        && header
            .split(|&c| c == b',')
            .any(|part| part.len() > ffi::FULLNAME_CAPACITY)
    {
        error::set(req, Code::ILLEGAL_ARGUMENT, "locale string too long")?;
        return Ok(OrFalse::False);
    }
    let mut status = Code::ZERO;
    let available = uloc::available(None, &mut status);
    or_false(error::check(req, status, "failed to retrieve locale list"))?;
    let Some(mut available) = available else {
        return Ok(OrFalse::False);
    };
    let (found, len, accepted, status) =
        uloc::accept(&mut available, &c_string(header), MAX_LOCALE_LEN);
    drop(available);
    if let OrFalse::False = or_false(error::check(
        req,
        status,
        "failed to find acceptable locale",
    ))? {
        return Ok(OrFalse::False);
    }
    if len < 0 || !accepted {
        return Ok(OrFalse::False);
    }
    PhpString::new(req, &found).map(OrFalse::Value)
}

/// `locale_add_likely_subtags` and `locale_minimize_subtags`.
fn subtags(
    req: &mut Request,
    locale: Path,
    part: Part,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let locale = or_default(req, locale.0.as_bytes());
    let (value, len, status) = uloc::fixed(part, &c_string(&locale));
    if let OrFalse::False = or_false(error::check(req, status, "invalid locale"))? {
        return Ok(OrFalse::False);
    }
    if len < 0 {
        return Ok(OrFalse::False);
    }
    PhpString::new(req, &value).map(OrFalse::Value)
}

/// `locale_get_default`.
fn get_default(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let locale = crate::default_locale(req);
    PhpString::new(req, &locale)
}

/// `locale_set_default`: `intl.default_locale` altered as
/// `zend_alter_ini_entry` does, ICU's default for the empty string.
fn set_default(req: &mut Request, locale: Path) -> Result<bool, Thrown> {
    let locale = match locale.0.as_bytes() {
        b"" => ffi::default_locale().as_bytes().to_vec(),
        locale => locale.to_vec(),
    };
    req.ini_mut().set(crate::DEFAULT_LOCALE, &locale)?;
    Ok(true)
}

/// `locale_is_right_to_left`.
fn is_right_to_left(req: &mut Request, locale: Path) -> bool {
    let locale = or_default(req, locale.0.as_bytes());
    uloc::is_right_to_left(&c_string(&locale))
}

/// The display natives' locale argument.
type Display = Option<Path>;

/// `locale_get_default(): string`.
///
/// ```php
/// ini_set('intl.default_locale', 'de_DE'); var_dump(locale_get_default());
/// ```
/// ```expect
/// string(5) "de_DE"
/// ```
#[php_function]
fn locale_get_default(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    get_default(req)
}

/// `locale_set_default(string $locale): true`.
///
/// ```php
/// var_dump(locale_set_default('fr_CA'), ini_get('intl.default_locale'), locale_get_default());
/// ```
/// ```expect
/// bool(true)
/// string(5) "fr_CA"
/// string(5) "fr_CA"
/// ```
#[php_function]
fn locale_set_default(req: &mut Request, locale: Path) -> Result<True, Thrown> {
    set_default(req, locale).map(|_| True)
}

/// `locale_get_primary_language(string $locale): ?string`.
///
/// ```php
/// var_dump(locale_get_primary_language('sr_Latn_RS'), locale_get_primary_language('i-klingon'), locale_get_primary_language(str_repeat('a', 157)), intl_get_error_message());
/// ```
/// ```expect
/// string(2) "sr"
/// string(9) "i-klingon"
/// NULL
/// string(120) "locale_get_primary_language(): Locale string too long, should be no longer than 156 characters: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn locale_get_primary_language(
    req: &mut Request,
    locale: Path,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    get_value(req, locale, Some(LANG))
}

/// `locale_get_script(string $locale): ?string`.
///
/// ```php
/// var_dump(locale_get_script('sr_Latn_RS'), locale_get_script('en_US'), locale_get_script('i-klingon'), intl_get_error_message());
/// ```
/// ```expect
/// string(4) "Latn"
/// string(0) ""
/// NULL
/// string(62) "locale_get_script(): unable to get locale script: U_ZERO_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn locale_get_script(req: &mut Request, locale: Path) -> Result<Option<Owner<PhpString>>, Thrown> {
    get_value(req, locale, Some(SCRIPT))
}

/// `locale_get_region(string $locale): ?string`.
///
/// ```php
/// var_dump(locale_get_region('sr_Latn_RS'), locale_get_region('de'));
/// ```
/// ```expect
/// string(2) "RS"
/// string(0) ""
/// ```
#[php_function(before_args = error::reset)]
fn locale_get_region(req: &mut Request, locale: Path) -> Result<Option<Owner<PhpString>>, Thrown> {
    get_value(req, locale, Some(REGION))
}

/// `locale_get_keywords(string $locale): array|false|null`.
///
/// ```php
/// var_dump(locale_get_keywords('de_DE@currency=EUR;collation=PHONEBOOK'), locale_get_keywords('de_DE'));
/// ```
/// ```expect
/// array(2) {
///   ["collation"]=>
///   string(9) "PHONEBOOK"
///   ["currency"]=>
///   string(3) "EUR"
/// }
/// NULL
/// ```
#[php_function(before_args = error::reset)]
fn locale_get_keywords(
    req: &mut Request,
    locale: Path,
) -> Result<Option<OrFalse<Owner<PhpArray>>>, Thrown> {
    get_keywords(req, locale)
}

/// `locale_get_display_script(string $locale, ?string $displayLocale = null): string|false`.
///
/// ```php
/// var_dump(locale_get_display_script('sr_Cyrl_RS', 'en'), locale_get_display_script('i-klingon', 'en'));
/// ```
/// ```expect
/// string(8) "Cyrillic"
/// bool(false)
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn locale_get_display_script(
    req: &mut Request,
    locale: Path,
    #[php(default = null)] displayLocale: Display,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    get_display(req, locale, displayLocale, Shown::Script, SCRIPT)
}

/// `locale_get_display_region(string $locale, ?string $displayLocale = null): string|false`.
///
/// ```php
/// var_dump(locale_get_display_region('de_CH', 'fr'), locale_get_display_region(str_repeat('a', 158)), intl_get_error_message());
/// ```
/// ```expect
/// string(6) "Suisse"
/// bool(false)
/// string(68) "locale_get_display_region(): name too long: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn locale_get_display_region(
    req: &mut Request,
    locale: Path,
    #[php(default = null)] displayLocale: Display,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    get_display(req, locale, displayLocale, Shown::Country, REGION)
}

/// `locale_get_display_name(string $locale, ?string $displayLocale = null): string|false`.
///
/// ```php
/// var_dump(locale_get_display_name('sl_IT_nedis', 'en'), locale_get_display_name('i-klingon', 'de'));
/// ```
/// ```expect
/// string(35) "Slovenian (Italy, Natisone dialect)"
/// string(11) "Klingonisch"
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn locale_get_display_name(
    req: &mut Request,
    locale: Path,
    #[php(default = null)] displayLocale: Display,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    get_display(req, locale, displayLocale, Shown::Name, "name")
}

/// `locale_get_display_language(string $locale, ?string $displayLocale = null): string|false`.
///
/// ```php
/// var_dump(locale_get_display_language('fr_CA', 'de'), locale_get_display_language('i-klingon', 'en'));
/// ```
/// ```expect
/// string(12) "Französisch"
/// string(7) "Klingon"
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn locale_get_display_language(
    req: &mut Request,
    locale: Path,
    #[php(default = null)] displayLocale: Display,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    get_display(req, locale, displayLocale, Shown::Language, LANG)
}

/// `locale_get_display_variant(string $locale, ?string $displayLocale = null): string|false`.
///
/// ```php
/// var_dump(locale_get_display_variant('sl_IT_ROZAJ', 'en'));
/// ```
/// ```expect
/// string(6) "Resian"
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn locale_get_display_variant(
    req: &mut Request,
    locale: Path,
    #[php(default = null)] displayLocale: Display,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    get_display(req, locale, displayLocale, Shown::Variant, VARIANT)
}

/// `locale_compose(array $subtags): string|false`.
///
/// ```php
/// var_dump(locale_compose(['language' => 'sl', 'script' => 'Latn', 'region' => 'IT', 'variant' => ['nedis', 'rozaj'], 'private1' => 'prv1', 'private2' => 'prv2']));
/// var_dump(locale_compose(['grandfathered' => 'i-klingon', 'language' => 'x']), locale_compose(['language' => 'de', 'region' => 1]), intl_get_error_message());
/// try { locale_compose(['script' => 'Latn']); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(34) "sl_Latn_IT_nedis_rozaj_x_prv1_prv2"
/// string(9) "i-klingon"
/// bool(false)
/// string(83) "locale_compose(): parameter array element is not a string: U_ILLEGAL_ARGUMENT_ERROR"
/// locale_compose(): Argument #1 ($subtags) must contain a "language" key
/// ```
#[php_function(before_args = error::reset)]
fn locale_compose(
    req: &mut Request,
    subtags: PhpArray,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    compose(req, subtags)
}

/// `locale_parse(string $locale): ?array`.
///
/// ```php
/// var_dump(locale_parse('sl_Latn_IT_nedis_rozaj-x-prv1-prv2'), locale_parse('i-klingon'));
/// ```
/// ```expect
/// array(7) {
///   ["language"]=>
///   string(2) "sl"
///   ["script"]=>
///   string(4) "Latn"
///   ["region"]=>
///   string(2) "IT"
///   ["variant0"]=>
///   string(5) "NEDIS"
///   ["variant1"]=>
///   string(5) "ROZAJ"
///   ["private0"]=>
///   string(4) "prv1"
///   ["private1"]=>
///   string(4) "prv2"
/// }
/// array(1) {
///   ["grandfathered"]=>
///   string(9) "i-klingon"
/// }
/// ```
#[php_function(before_args = error::reset)]
fn locale_parse(req: &mut Request, locale: Path) -> Result<Option<Owner<PhpArray>>, Thrown> {
    parse(req, locale)
}

/// `locale_get_all_variants(string $locale): ?array`.
///
/// ```php
/// var_dump(locale_get_all_variants('sl_IT_nedis_rozaj'), locale_get_all_variants('zh-min'));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(5) "NEDIS"
///   [1]=>
///   string(5) "ROZAJ"
/// }
/// array(0) {
/// }
/// ```
#[php_function(before_args = error::reset)]
fn locale_get_all_variants(
    req: &mut Request,
    locale: Path,
) -> Result<Option<Owner<PhpArray>>, Thrown> {
    all_variants(req, locale)
}

/// `locale_filter_matches(string $languageTag, string $locale, bool $canonicalize = false): ?bool`.
///
/// ```php
/// var_dump(locale_filter_matches('de-DEVA', 'de-DE'), locale_filter_matches('de_DE_1901', 'DE-de'), locale_filter_matches('de-DE', 'de-DE-1996', true), locale_filter_matches('x', '*'));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn locale_filter_matches(
    req: &mut Request,
    languageTag: Path,
    locale: Path,
    #[php(default = false)] canonicalize: bool,
) -> Result<Option<bool>, Thrown> {
    filter_matches(req, languageTag, locale, canonicalize)
}

/// `locale_canonicalize(string $locale): ?string`.
///
/// ```php
/// var_dump(locale_canonicalize('de-DE-1901'), locale_canonicalize('sr_Latn-RS@currency=EUR'));
/// ```
/// ```expect
/// string(10) "de_DE_1901"
/// string(23) "sr_Latn_RS@currency=EUR"
/// ```
#[php_function(before_args = error::reset)]
fn locale_canonicalize(
    req: &mut Request,
    locale: Path,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    get_value(req, locale, None)
}

/// `locale_lookup(array $languageTag, string $locale, bool $canonicalize = false, ?string $defaultLocale = null): ?string`.
///
/// ```php
/// var_dump(locale_lookup(['de-DEVA', 'de-DE-1996', 'de', 'de-De'], 'de-DE-1996-x-prv1-prv2'), locale_lookup(['en'], 'fr', false, 'xx'), locale_lookup(['de-DE'], 'DE_de', true));
/// try { locale_lookup([1], 'de'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(10) "de-DE-1996"
/// string(2) "xx"
/// string(5) "de_de"
/// locale_lookup(): Argument #2 ($locale) must only contain string values
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn locale_lookup(
    req: &mut Request,
    languageTag: PhpArray,
    locale: Path,
    #[php(default = false)] canonicalize: bool,
    #[php(default = null)] defaultLocale: Option<Path>,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    lookup(req, languageTag, locale, canonicalize, defaultLocale)
}

/// `locale_accept_from_http(string $header): string|false`.
///
/// ```php
/// var_dump(locale_accept_from_http('en-us,en;q=0.8'), locale_accept_from_http('xx'), locale_accept_from_http(str_repeat('x', 200)));
/// ```
/// ```expect
/// string(5) "en_US"
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn locale_accept_from_http(
    req: &mut Request,
    header: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    accept_from_http(req, header)
}

/// `locale_is_right_to_left(string $locale): bool`.
///
/// ```php
/// var_dump(locale_is_right_to_left('ar'), locale_is_right_to_left('en-US'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn locale_is_right_to_left(req: &mut Request, locale: Path) -> bool {
    is_right_to_left(req, locale)
}

/// `locale_add_likely_subtags(string $locale): string|false`.
///
/// ```php
/// var_dump(locale_add_likely_subtags('en'), locale_add_likely_subtags(str_repeat('a', 200)), intl_get_error_message());
/// ```
/// ```expect
/// string(10) "en_Latn_US"
/// bool(false)
/// string(%d) "locale_add_likely_subtags(): invalid locale: %s"
/// ```
#[php_function]
fn locale_add_likely_subtags(
    req: &mut Request,
    locale: Path,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    subtags(req, locale, Part::Likely)
}

/// `locale_minimize_subtags(string $locale): string|false`.
///
/// ```php
/// var_dump(locale_minimize_subtags('en_Latn_US'));
/// ```
/// ```expect
/// string(2) "en"
/// ```
#[php_function]
fn locale_minimize_subtags(
    req: &mut Request,
    locale: Path,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    subtags(req, locale, Part::Minimal)
}

/// `Locale`'s constants in `locale.stub.php`'s order, `DEFAULT_LOCALE`
/// typed `null`, which no Rust constant type declares.
fn class_constants() -> Vec<ClassConstDecl> {
    let int = |n| (Type::Int, ConstExpr::long(n));
    let string = |s: &str| (Type::String, ConstExpr::Str(s.as_bytes().into()));
    [
        ("ACTUAL_LOCALE", int(0)),
        ("VALID_LOCALE", int(1)),
        ("DEFAULT_LOCALE", (Type::Null, ConstExpr::Null)),
        ("LANG_TAG", string(LANG)),
        ("EXTLANG_TAG", string(EXTLANG)),
        ("SCRIPT_TAG", string(SCRIPT)),
        ("REGION_TAG", string(REGION)),
        ("VARIANT_TAG", string(VARIANT)),
        ("GRANDFATHERED_LANG_TAG", string(GRANDFATHERED_TAG)),
        ("PRIVATE_TAG", string(PRIVATE)),
    ]
    .into_iter()
    .map(|(name, (ty, value))| ClassConstDecl {
        name: Name(name.as_bytes().into()),
        visibility: Visibility::Public,
        is_final: false,
        is_case: false,
        ty: Some(ty),
        value,
        attributes: Vec::new().into(),
        doc_comment: None,
    })
    .collect()
}

/// The `locale_*` block of `php_intl.stub.php`, in its order, and
/// `Locale`'s constants.
pub(crate) fn functions(b: ExtensionBuilder) -> ExtensionBuilder {
    b.class_constants(b"Locale", class_constants()).functions(&[
        locale_get_default::DECLARED,
        locale_set_default::DECLARED,
        locale_get_primary_language::DECLARED,
        locale_get_script::DECLARED,
        locale_get_region::DECLARED,
        locale_get_keywords::DECLARED,
        locale_get_display_script::DECLARED,
        locale_get_display_region::DECLARED,
        locale_get_display_name::DECLARED,
        locale_get_display_language::DECLARED,
        locale_get_display_variant::DECLARED,
        locale_compose::DECLARED,
        locale_parse::DECLARED,
        locale_get_all_variants::DECLARED,
        locale_filter_matches::DECLARED,
        locale_canonicalize::DECLARED,
        locale_lookup::DECLARED,
        locale_accept_from_http::DECLARED,
        locale_is_right_to_left::DECLARED,
        locale_add_likely_subtags::DECLARED,
        locale_minimize_subtags::DECLARED,
    ])
}

/// `class Locale`: its constants and its static methods, each the
/// `locale_*` function php aliases it to.
///
/// ```php
/// var_dump(Locale::ACTUAL_LOCALE, Locale::VALID_LOCALE, Locale::DEFAULT_LOCALE, Locale::LANG_TAG, Locale::EXTLANG_TAG, Locale::SCRIPT_TAG, Locale::REGION_TAG, Locale::VARIANT_TAG, Locale::GRANDFATHERED_LANG_TAG, Locale::PRIVATE_TAG);
/// Locale::setDefault('sr_Latn_RS@currency=EUR');
/// echo Locale::getDefault(), ' ', Locale::getPrimaryLanguage(''), ' ', Locale::getScript(''), ' ', Locale::getRegion(''), ' ', Locale::canonicalize('sr-Latn'), "\n";
/// echo json_encode([Locale::getKeywords(''), Locale::parseLocale(''), Locale::getAllVariants('de_DE_1901'), Locale::composeLocale(['language' => 'de', 'region' => 'AT'])]), "\n";
/// echo Locale::getDisplayScript('', 'en'), ' ', Locale::getDisplayRegion('', 'en'), ' ', Locale::getDisplayName('de_AT', 'en'), ' ', Locale::getDisplayLanguage('de', 'en'), ' ', Locale::getDisplayVariant('de_DE_1901', 'en'), "\n";
/// var_dump(Locale::filterMatches('de-DE', 'de'), Locale::lookup(['de', 'en'], 'de_AT'), Locale::acceptFromHttp('de-AT'), Locale::isRightToLeft('he'), Locale::addLikelySubtags('sr'), Locale::minimizeSubtags('sr_Cyrl_RS'));
/// ```
/// ```expect
/// int(0)
/// int(1)
/// NULL
/// string(8) "language"
/// string(7) "extlang"
/// string(6) "script"
/// string(6) "region"
/// string(7) "variant"
/// string(13) "grandfathered"
/// string(7) "private"
/// sr_Latn_RS@currency=EUR sr Latn RS sr_Latn
/// [{"currency":"EUR"},{"language":"sr","script":"Latn","region":"RS"},["1901"],"de_AT"]
/// Latin Serbia German (Austria) German Traditional German orthography
/// bool(true)
/// string(2) "de"
/// string(5) "de_AT"
/// bool(true)
/// string(10) "sr_Cyrl_RS"
/// string(2) "sr"
/// ```
#[php_class]
pub(crate) struct Locale;

#[php_class]
#[allow(non_snake_case)]
impl Locale {
    #[php(tentative)]
    fn getDefault(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
        get_default(req)
    }

    #[php(tentative)]
    fn setDefault(req: &mut Request, locale: Path) -> Result<True, Thrown> {
        set_default(req, locale).map(|_| True)
    }

    #[php(tentative, before_args = error::reset)]
    fn getPrimaryLanguage(
        req: &mut Request,
        locale: Path,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        get_value(req, locale, Some(LANG))
    }

    #[php(tentative, before_args = error::reset)]
    fn getScript(req: &mut Request, locale: Path) -> Result<Option<Owner<PhpString>>, Thrown> {
        get_value(req, locale, Some(SCRIPT))
    }

    #[php(tentative, before_args = error::reset)]
    fn getRegion(req: &mut Request, locale: Path) -> Result<Option<Owner<PhpString>>, Thrown> {
        get_value(req, locale, Some(REGION))
    }

    #[php(tentative, before_args = error::reset)]
    fn getKeywords(
        req: &mut Request,
        locale: Path,
    ) -> Result<Option<OrFalse<Owner<PhpArray>>>, Thrown> {
        get_keywords(req, locale)
    }

    #[php(tentative, before_args = error::reset)]
    fn getDisplayScript(
        req: &mut Request,
        locale: Path,
        #[php(default = null)] displayLocale: Display,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        get_display(req, locale, displayLocale, Shown::Script, SCRIPT)
    }

    #[php(tentative, before_args = error::reset)]
    fn getDisplayRegion(
        req: &mut Request,
        locale: Path,
        #[php(default = null)] displayLocale: Display,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        get_display(req, locale, displayLocale, Shown::Country, REGION)
    }

    #[php(tentative, before_args = error::reset)]
    fn getDisplayName(
        req: &mut Request,
        locale: Path,
        #[php(default = null)] displayLocale: Display,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        get_display(req, locale, displayLocale, Shown::Name, "name")
    }

    #[php(tentative, before_args = error::reset)]
    fn getDisplayLanguage(
        req: &mut Request,
        locale: Path,
        #[php(default = null)] displayLocale: Display,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        get_display(req, locale, displayLocale, Shown::Language, LANG)
    }

    #[php(tentative, before_args = error::reset)]
    fn getDisplayVariant(
        req: &mut Request,
        locale: Path,
        #[php(default = null)] displayLocale: Display,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        get_display(req, locale, displayLocale, Shown::Variant, VARIANT)
    }

    #[php(tentative, before_args = error::reset)]
    fn composeLocale(
        req: &mut Request,
        subtags: PhpArray,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        compose(req, subtags)
    }

    #[php(tentative, before_args = error::reset)]
    fn parseLocale(req: &mut Request, locale: Path) -> Result<Option<Owner<PhpArray>>, Thrown> {
        parse(req, locale)
    }

    #[php(tentative, before_args = error::reset)]
    fn getAllVariants(req: &mut Request, locale: Path) -> Result<Option<Owner<PhpArray>>, Thrown> {
        all_variants(req, locale)
    }

    #[php(tentative, before_args = error::reset)]
    fn filterMatches(
        req: &mut Request,
        languageTag: Path,
        locale: Path,
        #[php(default = false)] canonicalize: bool,
    ) -> Result<Option<bool>, Thrown> {
        filter_matches(req, languageTag, locale, canonicalize)
    }

    #[php(tentative, before_args = error::reset)]
    fn lookup(
        req: &mut Request,
        languageTag: PhpArray,
        locale: Path,
        #[php(default = false)] canonicalize: bool,
        #[php(default = null)] defaultLocale: Option<Path>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        lookup(req, languageTag, locale, canonicalize, defaultLocale)
    }

    #[php(tentative, before_args = error::reset)]
    fn canonicalize(req: &mut Request, locale: Path) -> Result<Option<Owner<PhpString>>, Thrown> {
        get_value(req, locale, None)
    }

    #[php(tentative)]
    fn acceptFromHttp(
        req: &mut Request,
        header: PhpString,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        accept_from_http(req, header)
    }

    fn isRightToLeft(req: &mut Request, locale: Path) -> bool {
        is_right_to_left(req, locale)
    }

    fn addLikelySubtags(
        req: &mut Request,
        locale: Path,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        subtags(req, locale, Part::Likely)
    }

    fn minimizeSubtags(
        req: &mut Request,
        locale: Path,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        subtags(req, locale, Part::Minimal)
    }
}
