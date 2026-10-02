//! `uchar.h`: the character properties `IntlChar` answers. What takes and
//! answers integers alone is total over every input, so it is declared
//! `safe`; names, enumerations and closures cross buffers here.

use std::ffi::{CStr, c_char, c_void};

use super::{Code, c_string, fill_chars, fill_utf16, static_str};

/// The integer-only calls, each total over its arguments (`UBool` as
/// `i8`, `UProperty` and the other enums as `i32`).
macro_rules! total {
    ($( fn $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty; )*) => {
        unsafe extern "C" { $(
            #[link_name = concat!(stringify!($name), env!("INTL_ICU_SUFFIX"))]
            pub(crate) safe fn $name($($arg: $ty),*) -> $ret;
        )* }
    };
}

total! {
    fn u_hasBinaryProperty(c: i32, which: i32) -> i8;
    fn u_getIntPropertyValue(c: i32, which: i32) -> i32;
    fn u_getIntPropertyMinValue(which: i32) -> i32;
    fn u_getIntPropertyMaxValue(which: i32) -> i32;
    fn u_getNumericValue(c: i32) -> f64;
    fn ublock_getCode(c: i32) -> i32;
    fn u_foldCase(c: i32, options: u32) -> i32;
    fn u_digit(c: i32, radix: i8) -> i32;
    fn u_forDigit(digit: i32, radix: i8) -> i32;
    fn u_isUAlphabetic(c: i32) -> i8;
    fn u_isULowercase(c: i32) -> i8;
    fn u_isUUppercase(c: i32) -> i8;
    fn u_isUWhiteSpace(c: i32) -> i8;
    fn u_islower(c: i32) -> i8;
    fn u_isupper(c: i32) -> i8;
    fn u_istitle(c: i32) -> i8;
    fn u_isdigit(c: i32) -> i8;
    fn u_isalpha(c: i32) -> i8;
    fn u_isalnum(c: i32) -> i8;
    fn u_isxdigit(c: i32) -> i8;
    fn u_ispunct(c: i32) -> i8;
    fn u_isgraph(c: i32) -> i8;
    fn u_isblank(c: i32) -> i8;
    fn u_isdefined(c: i32) -> i8;
    fn u_isspace(c: i32) -> i8;
    fn u_isJavaSpaceChar(c: i32) -> i8;
    fn u_isWhitespace(c: i32) -> i8;
    fn u_iscntrl(c: i32) -> i8;
    fn u_isISOControl(c: i32) -> i8;
    fn u_isprint(c: i32) -> i8;
    fn u_isbase(c: i32) -> i8;
    fn u_isMirrored(c: i32) -> i8;
    fn u_isIDStart(c: i32) -> i8;
    fn u_isIDPart(c: i32) -> i8;
    fn u_isIDIgnorable(c: i32) -> i8;
    fn u_isJavaIDStart(c: i32) -> i8;
    fn u_isJavaIDPart(c: i32) -> i8;
    fn u_charDirection(c: i32) -> i32;
    fn u_charType(c: i32) -> i8;
    fn u_getCombiningClass(c: i32) -> u8;
    fn u_charDigitValue(c: i32) -> i32;
    fn u_charMirror(c: i32) -> i32;
    fn u_tolower(c: i32) -> i32;
    fn u_toupper(c: i32) -> i32;
    fn u_totitle(c: i32) -> i32;
    fn u_getBidiPairedBracket(c: i32) -> i32;
}

/// `UEnumCharNamesFn`.
type NamesFn = extern "C" fn(*mut c_void, i32, i32, *const c_char, i32) -> i8;
/// `UCharEnumTypeRange`.
type TypesFn = extern "C" fn(*const c_void, i32, i32, i32) -> i8;

mod sys {
    use std::ffi::{c_char, c_void};

    use super::super::Code;
    use super::{NamesFn, TypesFn};

    icu! {
        fn u_charAge(c: i32, version: *mut u8);
        fn u_getUnicodeVersion(version: *mut u8);
        fn u_charName(c: i32, choice: i32, buffer: *mut c_char, capacity: i32, status: &mut Code) -> i32;
        fn u_charFromName(choice: i32, name: *const c_char, status: &mut Code) -> i32;
        fn u_enumCharNames(
            start: i32,
            limit: i32,
            each: NamesFn,
            data: *mut c_void,
            choice: i32,
            status: &mut Code,
        );
        fn u_enumCharTypes(each: TypesFn, data: *const c_void);
        fn u_getPropertyName(property: i32, choice: i32) -> *const c_char;
        fn u_getPropertyEnum(alias: *const c_char) -> i32;
        fn u_getPropertyValueName(property: i32, value: i32, choice: i32) -> *const c_char;
        fn u_getPropertyValueEnum(property: i32, alias: *const c_char) -> i32;
        fn u_getFC_NFKC_Closure(c: i32, dest: *mut u16, capacity: i32, status: &mut Code) -> i32;
    }
}

/// `u_charAge`, or `u_getUnicodeVersion` for `None`.
pub(crate) fn version(c: Option<i32>) -> [u8; 4] {
    let mut version = [0u8; 4];
    match c {
        // SAFETY: ICU writes U_MAX_VERSION_LENGTH, four, bytes.
        Some(c) => unsafe { sys::u_charAge(c, version.as_mut_ptr()) },
        // SAFETY: as above.
        None => unsafe { sys::u_getUnicodeVersion(version.as_mut_ptr()) },
    }
    version
}

/// `u_charName`, preflighted.
pub(crate) fn char_name(c: i32, choice: i32) -> Result<Vec<u8>, Code> {
    let (name, status) = fill_chars(|buffer, capacity, status| {
        // SAFETY: the buffer holds capacity bytes and ICU writes no further.
        unsafe { sys::u_charName(c, choice, buffer, capacity, status) }
    });
    status.check().map(|_| name)
}

/// `u_charFromName`, with the code ICU set.
pub(crate) fn char_from_name(choice: i32, name: &[u8]) -> (i32, Code) {
    let name = c_string(name);
    let mut status = Code::ZERO;
    // SAFETY: the name is NUL-terminated.
    let c = unsafe { sys::u_charFromName(choice, name.as_ptr(), &mut status) };
    (c, status)
}

extern "C" fn each_name(data: *mut c_void, c: i32, _: i32, name: *const c_char, len: i32) -> i8 {
    // SAFETY: data is the Vec `char_names` handed ICU, alive for the call.
    let names = unsafe { &mut *data.cast::<Vec<(i32, Vec<u8>)>>() };
    let len = usize::try_from(len).unwrap_or(0);
    // SAFETY: ICU hands len bytes of the name, valid for this call.
    let name = unsafe { std::slice::from_raw_parts(name.cast::<u8>(), len) };
    names.push((c, name.to_vec()));
    1
}

/// `u_enumCharNames`: every name in `start..limit`, collected.
pub(crate) fn char_names(start: i32, limit: i32, choice: i32) -> (Vec<(i32, Vec<u8>)>, Code) {
    let mut names: Vec<(i32, Vec<u8>)> = Vec::new();
    let mut status = Code::ZERO;
    let data: *mut c_void = (&raw mut names).cast();
    // SAFETY: the callback reads data as the Vec it is, for this call alone.
    unsafe { sys::u_enumCharNames(start, limit, each_name, data, choice, &mut status) };
    (names, status)
}

extern "C" fn each_type(data: *const c_void, start: i32, limit: i32, kind: i32) -> i8 {
    // SAFETY: data is the Vec `char_types` handed ICU, alive for the call.
    let ranges = unsafe { &mut *data.cast_mut().cast::<Vec<[i32; 3]>>() };
    ranges.push([start, limit, kind]);
    1
}

/// `u_enumCharTypes`: every range and its category, collected.
pub(crate) fn char_types() -> Vec<[i32; 3]> {
    let mut ranges: Vec<[i32; 3]> = Vec::new();
    let data: *const c_void = (&raw mut ranges).cast_const().cast();
    // SAFETY: the callback reads data as the Vec it is, for this call alone.
    unsafe { sys::u_enumCharTypes(each_type, data) };
    ranges
}

/// `u_getPropertyName`, or `u_getPropertyValueName` with a value.
pub(crate) fn property_name(
    property: i32,
    value: Option<i32>,
    choice: i32,
) -> Option<&'static str> {
    let name = match value {
        // SAFETY: the call answers a static name or null.
        Some(value) => unsafe { sys::u_getPropertyValueName(property, value, choice) },
        // SAFETY: as above.
        None => unsafe { sys::u_getPropertyName(property, choice) },
    };
    static_str(name)
}

/// `u_getPropertyEnum`, or `u_getPropertyValueEnum` under a property.
pub(crate) fn property_enum(property: Option<i32>, alias: &[u8]) -> i32 {
    let alias = c_string(alias);
    let alias: &CStr = &alias;
    match property {
        // SAFETY: the alias is NUL-terminated.
        Some(property) => unsafe { sys::u_getPropertyValueEnum(property, alias.as_ptr()) },
        // SAFETY: as above.
        None => unsafe { sys::u_getPropertyEnum(alias.as_ptr()) },
    }
}

/// `u_getFC_NFKC_Closure`, preflighted, with the code ICU set.
pub(crate) fn fc_nfkc_closure(c: i32) -> (Vec<u16>, Code) {
    fill_utf16(|dest, capacity, status| {
        // SAFETY: dest holds capacity units and ICU writes no further.
        unsafe { sys::u_getFC_NFKC_Closure(c, dest, capacity, status) }
    })
}
