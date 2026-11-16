//! `uchar/`: the `IntlChar` class over ICU's `uchar.h` (`uchar.cpp`),
//! every method static, each resetting the request's error before its
//! arguments are held (`before_args = error::reset`). A code point is
//! taken as `int|string`, a string being exactly one UTF-8 code
//! point (`convert_cp`); the case and mirror mappings answer in the form
//! they were handed.

use php_ext::abi::{Callable, IntoConst, OrFalse, Union};
use php_ext::attr::php_class;
use php_ext::call::call;
use php_ext::decl::{ClassConstDecl, Name, Visibility};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use crate::error;
use crate::ffi::uchar::{self as icu, char_from_name, char_name, char_names, char_types};
use crate::ffi::{self, Code};
use crate::utf8;

/// `int|string $codepoint`, `Z_PARAM_STR_OR_LONG`.
type Codepoint = Union<PhpString, i64>;

/// A code point or the string it was handed as, `int|string|null`.
type Mapped = Result<Option<Union<Owner<PhpString>, i64>>, Thrown>;

/// `class IntlChar`: its constants in the stub's order, laid down by
/// [`constants`], and its static methods.
///
/// ```php
/// $is = ['isalnum', 'isalpha', 'isbase', 'isblank', 'iscntrl', 'isdefined', 'isdigit', 'isgraph', 'isIDIgnorable', 'isIDPart', 'isIDStart', 'isISOControl', 'isJavaIDPart', 'isJavaIDStart', 'isJavaSpaceChar', 'islower', 'isMirrored', 'isprint', 'ispunct', 'isspace', 'istitle', 'isUAlphabetic', 'isULowercase', 'isupper', 'isUUppercase', 'isUWhiteSpace', 'isWhitespace', 'isxdigit'];
/// foreach (['A', ' ', '(', "\t"] as $c) { foreach ($is as $m) echo (int) IntlChar::$m($c); echo "\n"; }
/// var_dump(IntlChar::isalpha(''), intl_get_error_message(), IntlChar::isalpha(0x110000), intl_get_error_message());
/// echo IntlChar::UNICODE_VERSION, ' ', IntlChar::NO_NUMERIC_VALUE, ' ', IntlChar::FOLD_CASE_EXCLUDE_SPECIAL_I, "\n";
/// ```
/// ```expect
/// 1110010101101100010001011001
/// 0001010000000010010100000110
/// 0000010100000000111000000000
/// 0001110000010000000100000110
/// NULL
/// string(147) "IntlChar::isalpha(): Passing a UTF-8 character for codepoint requires a string which is exactly one UTF-8 codepoint long.: U_ILLEGAL_ARGUMENT_ERROR"
/// NULL
/// string(69) "IntlChar::isalpha(): Codepoint out of range: U_ILLEGAL_ARGUMENT_ERROR"
/// 18.0 -123456789 1
/// ```
///
/// ```php
/// echo implode(' ', array_map(fn ($m) => IntlChar::$m("\u{663}"), ['charDigitValue', 'charDirection', 'charType', 'getBlockCode', 'getCombiningClass', 'ord'])), "\n";
/// var_dump(IntlChar::charMirror('('), IntlChar::tolower(65), IntlChar::toupper('a'), IntlChar::totitle("\u{1C6}"), IntlChar::getBidiPairedBracket(0x5B), IntlChar::foldCase('I', IntlChar::FOLD_CASE_EXCLUDE_SPECIAL_I), IntlChar::foldCase(0x49));
/// var_dump(IntlChar::chr(0x2603), IntlChar::chr('x'), IntlChar::digit('7'), IntlChar::digit('a', 16), IntlChar::digit('z'), IntlChar::forDigit(11, 16), IntlChar::getNumericValue("\u{BD}"));
/// echo implode('.', IntlChar::charAge("\u{1F600}")), ' ', count(IntlChar::getUnicodeVersion()), "\n";
/// ```
/// ```expect
/// 3 5 9 12 0 1635
/// string(1) ")"
/// int(97)
/// string(1) "A"
/// string(2) "ǅ"
/// int(93)
/// string(2) "ı"
/// int(105)
/// string(3) "☃"
/// string(1) "x"
/// int(7)
/// int(10)
/// bool(false)
/// int(98)
/// float(0.5)
/// 6.1.0.0 4
/// ```
///
/// ```php
/// var_dump(IntlChar::charName('A'), IntlChar::charName('A', 12345), IntlChar::charFromName('SNOWMAN'), IntlChar::charFromName('nope'), intl_get_error_code());
/// var_dump(IntlChar::getPropertyName(IntlChar::PROPERTY_ALPHABETIC), IntlChar::getPropertyName(-5), IntlChar::getPropertyEnum('Alpha'));
/// var_dump(IntlChar::getPropertyValueName(IntlChar::PROPERTY_BLOCK, 1, IntlChar::SHORT_PROPERTY_NAME), IntlChar::getPropertyValueEnum(IntlChar::PROPERTY_BLOCK, 'ASCII'));
/// var_dump(IntlChar::hasBinaryProperty('A', IntlChar::PROPERTY_ALPHABETIC), IntlChar::getIntPropertyValue('A', IntlChar::PROPERTY_BLOCK), IntlChar::getIntPropertyMinValue(IntlChar::PROPERTY_BLOCK), IntlChar::getIntPropertyMaxValue(IntlChar::PROPERTY_BIDI_PAIRED_BRACKET_TYPE));
/// var_dump(bin2hex(IntlChar::getFC_NFKC_Closure("\u{2121}")), IntlChar::getFC_NFKC_Closure('a'));
/// IntlChar::enumCharNames(0x2600, 0x2603, function ($c, $type, $name) { echo dechex($c), " $type $name\n"; });
/// var_dump(IntlChar::enumCharNames('', 1, 'strlen'));
/// $n = 0;
/// IntlChar::enumCharTypes(function ($start, $limit, $type) use (&$n) { if ($n++ < 2) echo "$start $limit $type\n"; });
/// ```
/// ```expect
/// string(22) "LATIN CAPITAL LETTER A"
/// NULL
/// int(9731)
/// NULL
/// int(12)
/// string(10) "Alphabetic"
/// bool(false)
/// int(0)
/// string(5) "ASCII"
/// int(1)
/// bool(true)
/// int(1)
/// int(0)
/// int(2)
/// string(6) "74656c"
/// string(0) ""
/// 2600 0 BLACK SUN WITH RAYS
/// 2601 0 CLOUD
/// 2602 0 UMBRELLA
/// bool(false)
/// 0 32 15
/// 32 33 12
/// ```
#[php_class]
pub(crate) struct IntlChar;

#[php_class]
#[allow(non_snake_case)]
impl IntlChar {
    #[php(tentative, before_args = error::reset)]
    fn hasBinaryProperty(
        req: &mut Request,
        codepoint: Codepoint,
        property: i64,
    ) -> Result<Option<bool>, Thrown> {
        let c = convert(req, &codepoint)?;
        Ok(c.map(|c| icu::u_hasBinaryProperty(c, int(property)) != 0))
    }

    #[php(tentative, before_args = error::reset)]
    fn charAge(req: &mut Request, codepoint: Codepoint) -> Result<Option<Owner<PhpArray>>, Thrown> {
        match convert(req, &codepoint)? {
            Some(c) => version(req, Some(c)).map(Some),
            None => Ok(None),
        }
    }

    #[php(tentative, before_args = error::reset)]
    fn charDigitValue(req: &mut Request, codepoint: Codepoint) -> Result<Option<i64>, Thrown> {
        long(req, codepoint, |c| icu::u_charDigitValue(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn charDirection(req: &mut Request, codepoint: Codepoint) -> Result<Option<i64>, Thrown> {
        long(req, codepoint, |c| icu::u_charDirection(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn charFromName(
        req: &mut Request,
        name: PhpString,
        #[php(default = IntlChar::UNICODE_CHAR_NAME)] r#type: i64,
    ) -> Option<i64> {
        let (c, status) = char_from_name(int(r#type), name.as_bytes());
        // php's INTL_CHECK_STATUS_OR_NULL with no message: the code alone.
        error::set_code(req, status);
        (!status.is_failure()).then_some(i64::from(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn charMirror(req: &mut Request, codepoint: Codepoint) -> Mapped {
        mapped(req, codepoint, |c| icu::u_charMirror(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn charName(
        req: &mut Request,
        codepoint: Codepoint,
        #[php(default = IntlChar::UNICODE_CHAR_NAME)] r#type: i64,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        let Some(c) = convert(req, &codepoint)? else {
            return Ok(None);
        };
        match char_name(c, int(r#type)) {
            Ok(name) => PhpString::new(req, &name).map(Some),
            Err(code) => {
                error::set(req, code, "Failure getting character name")?;
                Ok(None)
            }
        }
    }

    #[php(tentative, before_args = error::reset)]
    fn charType(req: &mut Request, codepoint: Codepoint) -> Result<Option<i64>, Thrown> {
        long(req, codepoint, |c| i32::from(icu::u_charType(c)))
    }

    #[php(tentative, before_args = error::reset)]
    fn chr(req: &mut Request, codepoint: Codepoint) -> Result<Option<Owner<PhpString>>, Thrown> {
        match convert(req, &codepoint)? {
            Some(c) => PhpString::new(req, &utf8::encode(c)).map(Some),
            None => Ok(None),
        }
    }

    #[php(tentative, before_args = error::reset)]
    fn digit(
        req: &mut Request,
        codepoint: Codepoint,
        #[php(default = 10)] base: i64,
    ) -> Result<Option<OrFalse<i64>>, Thrown> {
        let Some(c) = convert(req, &codepoint)? else {
            return Ok(None);
        };
        let digit = icu::u_digit(c, radix(base));
        if digit < 0 {
            error::set(req, Code::ILLEGAL_ARGUMENT, "Invalid digit")?;
            return Ok(Some(OrFalse::False));
        }
        Ok(Some(OrFalse::Value(i64::from(digit))))
    }

    #[php(tentative, before_args = error::reset)]
    fn enumCharNames(
        req: &mut Request,
        start: Codepoint,
        end: Codepoint,
        callback: Callable,
        #[php(default = IntlChar::UNICODE_CHAR_NAME)] r#type: i64,
    ) -> Result<bool, Thrown> {
        let Some(start) = convert(req, &start)? else {
            return Ok(false);
        };
        let Some(limit) = convert(req, &end)? else {
            return Ok(false);
        };
        let choice = int(r#type);
        let (names, status) = char_names(start, limit, choice);
        for (c, name) in names {
            let name = PhpString::new(req, &name)?;
            let args = [
                Value::int(i64::from(c)),
                Value::int(i64::from(choice)),
                Value::string(*name),
            ];
            let answer = call(req, &callback, &args);
            name.give_up(req);
            answer?.give_up(req);
        }
        error::set_code(req, status);
        Ok(!status.is_failure())
    }

    #[php(tentative, before_args = error::reset)]
    fn enumCharTypes(req: &mut Request, callback: Callable) -> Result<(), Thrown> {
        for range in char_types() {
            let args = range.map(|n| Value::int(i64::from(n)));
            let answer = call(req, &callback, &args)?;
            answer.give_up(req);
        }
        Ok(())
    }

    #[php(tentative, before_args = error::reset)]
    fn foldCase(
        req: &mut Request,
        codepoint: Codepoint,
        #[php(default = IntlChar::FOLD_CASE_DEFAULT)] options: i64,
    ) -> Mapped {
        // `uint32_t options`: the low half of the long, as C passes it.
        mapped(req, codepoint, |c| {
            icu::u_foldCase(c, u32::from_le_bytes(int(options).to_le_bytes()))
        })
    }

    #[php(tentative, before_args = error::reset)]
    fn forDigit(digit: i64, #[php(default = 10)] base: i64) -> i64 {
        i64::from(icu::u_forDigit(int(digit), radix(base)))
    }

    #[php(tentative, before_args = error::reset)]
    fn getBidiPairedBracket(req: &mut Request, codepoint: Codepoint) -> Mapped {
        mapped(req, codepoint, |c| icu::u_getBidiPairedBracket(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn getBlockCode(req: &mut Request, codepoint: Codepoint) -> Result<Option<i64>, Thrown> {
        long(req, codepoint, |c| icu::ublock_getCode(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn getCombiningClass(req: &mut Request, codepoint: Codepoint) -> Result<Option<i64>, Thrown> {
        long(req, codepoint, |c| i32::from(icu::u_getCombiningClass(c)))
    }

    #[php(tentative, before_args = error::reset)]
    fn getFC_NFKC_Closure(
        req: &mut Request,
        codepoint: Codepoint,
    ) -> Result<Option<OrFalse<Owner<PhpString>>>, Thrown> {
        let Some(c) = convert(req, &codepoint)? else {
            return Ok(None);
        };
        let (closure, status) = icu::fc_nfkc_closure(c);
        if closure.is_empty() && !status.is_failure() {
            return PhpString::new(req, b"").map(|s| Some(OrFalse::Value(s)));
        }
        if let Err(thrown) = error::check(req, status, "Failed getting closure") {
            return thrown.map_or(Ok(Some(OrFalse::False)), Err);
        }
        let (bytes, status) = match ffi::to_utf8(&closure) {
            Ok(bytes) => (bytes, Code::ZERO),
            Err(code) => (Vec::new(), code),
        };
        if let Err(thrown) = error::check(req, status, "Failed converting output to UTF8") {
            return thrown.map_or(Ok(Some(OrFalse::False)), Err);
        }
        PhpString::new(req, &bytes).map(|s| Some(OrFalse::Value(s)))
    }

    #[php(tentative, before_args = error::reset)]
    fn getIntPropertyMaxValue(property: i64) -> i64 {
        i64::from(icu::u_getIntPropertyMaxValue(int(property)))
    }

    #[php(tentative, before_args = error::reset)]
    fn getIntPropertyMinValue(property: i64) -> i64 {
        i64::from(icu::u_getIntPropertyMinValue(int(property)))
    }

    #[php(tentative, before_args = error::reset)]
    fn getIntPropertyValue(
        req: &mut Request,
        codepoint: Codepoint,
        property: i64,
    ) -> Result<Option<i64>, Thrown> {
        let c = convert(req, &codepoint)?;
        Ok(c.map(|c| i64::from(icu::u_getIntPropertyValue(c, int(property)))))
    }

    #[php(tentative, before_args = error::reset)]
    fn getNumericValue(req: &mut Request, codepoint: Codepoint) -> Result<Option<f64>, Thrown> {
        Ok(convert(req, &codepoint)?.map(|c| icu::u_getNumericValue(c)))
    }

    #[php(tentative, before_args = error::reset)]
    fn getPropertyEnum(alias: PhpString) -> i64 {
        i64::from(icu::property_enum(None, alias.as_bytes()))
    }

    #[php(tentative, before_args = error::reset)]
    fn getPropertyName(
        req: &mut Request,
        property: i64,
        #[php(default = IntlChar::LONG_PROPERTY_NAME)] r#type: i64,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        property_name(req, icu::property_name(int(property), None, int(r#type)))
    }

    #[php(tentative, before_args = error::reset)]
    fn getPropertyValueEnum(property: i64, name: PhpString) -> i64 {
        i64::from(icu::property_enum(Some(int(property)), name.as_bytes()))
    }

    #[php(tentative, before_args = error::reset)]
    fn getPropertyValueName(
        req: &mut Request,
        property: i64,
        value: i64,
        #[php(default = IntlChar::LONG_PROPERTY_NAME)] r#type: i64,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let name = icu::property_name(int(property), Some(int(value)), int(r#type));
        property_name(req, name)
    }

    #[php(tentative, before_args = error::reset)]
    fn getUnicodeVersion(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
        version(req, None)
    }

    #[php(tentative, before_args = error::reset)]
    fn isalnum(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isalnum(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isalpha(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isalpha(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isbase(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isbase(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isblank(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isblank(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn iscntrl(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_iscntrl(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isdefined(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isdefined(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isdigit(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isdigit(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isgraph(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isgraph(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isIDIgnorable(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isIDIgnorable(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isIDPart(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isIDPart(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isIDStart(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isIDStart(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isISOControl(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isISOControl(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isJavaIDPart(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isJavaIDPart(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isJavaIDStart(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isJavaIDStart(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isJavaSpaceChar(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isJavaSpaceChar(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn islower(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_islower(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isMirrored(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isMirrored(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isprint(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isprint(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn ispunct(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_ispunct(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isspace(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isspace(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn istitle(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_istitle(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isUAlphabetic(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isUAlphabetic(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isULowercase(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isULowercase(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isupper(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isupper(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isUUppercase(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isUUppercase(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isUWhiteSpace(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isUWhiteSpace(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isWhitespace(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isWhitespace(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn isxdigit(req: &mut Request, codepoint: Codepoint) -> Result<Option<bool>, Thrown> {
        test(req, codepoint, |c| icu::u_isxdigit(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn ord(req: &mut Request, character: Codepoint) -> Result<Option<i64>, Thrown> {
        long(req, character, |c| c)
    }

    #[php(tentative, before_args = error::reset)]
    fn tolower(req: &mut Request, codepoint: Codepoint) -> Mapped {
        mapped(req, codepoint, |c| icu::u_tolower(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn totitle(req: &mut Request, codepoint: Codepoint) -> Mapped {
        mapped(req, codepoint, |c| icu::u_totitle(c))
    }

    #[php(tentative, before_args = error::reset)]
    fn toupper(req: &mut Request, codepoint: Codepoint) -> Mapped {
        mapped(req, codepoint, |c| icu::u_toupper(c))
    }
}

/// A long as the `int32_t` (or C enum) C converts it to: its low half.
#[allow(clippy::cast_possible_truncation)]
fn int(n: i64) -> i32 {
    n as i32
}

/// A long as the `int8_t` radix ICU takes: its low byte.
#[allow(clippy::cast_possible_truncation)]
fn radix(n: i64) -> i8 {
    n as i8
}

/// `convert_cp`: the code point an int names or a one-code-point UTF-8
/// string holds, `None` with the request's error set where it is out of
/// range or the string holds other than one. Each of uchar.cpp's methods
/// resets the request's error ahead of its ZPP (`before_args`), so a
/// `TypeError` leaves no earlier failure standing.
///
/// ```php
/// IntlChar::isalpha(0x110000);
/// try { IntlChar::isalpha([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code());
/// IntlChar::chr(-1);
/// try { IntlChar::charName('A', 'x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code());
/// IntlChar::ord('ab');
/// try { IntlChar::getUnicodeVersion(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code());
/// IntlChar::tolower(-1);
/// try { IntlChar::forDigit('x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code());
/// ```
/// ```expect
/// IntlChar::isalpha(): Argument #1 ($codepoint) must be of type string|int, array given
/// int(0)
/// IntlChar::charName(): Argument #2 ($type) must be of type int, string given
/// int(0)
/// IntlChar::getUnicodeVersion() expects exactly 0 arguments, 1 given
/// int(0)
/// IntlChar::forDigit(): Argument #1 ($digit) must be of type int, string given
/// int(0)
/// ```
fn convert(req: &mut Request, codepoint: &Codepoint) -> Result<Option<i32>, Thrown> {
    let c = match codepoint {
        Union::A(text) => {
            let bytes = text.as_bytes();
            if ffi::length(bytes.len()).is_err() {
                error::set(req, Code::ILLEGAL_ARGUMENT, "Input string is too long.")?;
                return Ok(None);
            }
            let mut end = 0;
            let c = utf8::next(bytes, &mut end);
            if end != bytes.len() {
                let message = "Passing a UTF-8 character for codepoint requires a string which is exactly one UTF-8 codepoint long.";
                error::set(req, Code::ILLEGAL_ARGUMENT, message)?;
                return Ok(None);
            }
            i64::from(c)
        }
        Union::B(n) => *n,
    };
    if !(0..=0x10_ffff).contains(&c) {
        error::set(req, Code::ILLEGAL_ARGUMENT, "Codepoint out of range")?;
        return Ok(None);
    }
    Ok(i32::try_from(c).ok())
}

/// `IC_BOOL_METHOD_CHAR`.
fn test(
    req: &mut Request,
    codepoint: Codepoint,
    f: impl Fn(i32) -> i8,
) -> Result<Option<bool>, Thrown> {
    Ok(convert(req, &codepoint)?.map(|c| f(c) != 0))
}

/// `IC_INT_METHOD_CHAR`.
fn long(
    req: &mut Request,
    codepoint: Codepoint,
    f: impl Fn(i32) -> i32,
) -> Result<Option<i64>, Thrown> {
    Ok(convert(req, &codepoint)?.map(|c| i64::from(f(c))))
}

/// `IC_CHAR_METHOD_CHAR`: the mapping of a code point as a string where
/// a string was handed, else as an int.
fn mapped(req: &mut Request, codepoint: Codepoint, f: impl Fn(i32) -> i32) -> Mapped {
    let Some(c) = convert(req, &codepoint)? else {
        return Ok(None);
    };
    let c = f(c);
    Ok(Some(match codepoint {
        Union::A(_) => Union::A(PhpString::new(req, &utf8::encode(c))?),
        Union::B(_) => Union::B(i64::from(c)),
    }))
}

/// `u_charAge` or `u_getUnicodeVersion` as php's list of four ints.
fn version(req: &mut Request, c: Option<i32>) -> Result<Owner<PhpArray>, Thrown> {
    let mut list = PhpArray::new(req)?;
    for part in icu::version(c) {
        list.append(req, Owner::int(i64::from(part)))?;
    }
    Ok(list)
}

/// A property's or a value's name, or `false` with php's error.
fn property_name(
    req: &mut Request,
    name: Option<&str>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Some(name) = name else {
        error::set(req, Code::ILLEGAL_ARGUMENT, "Failed to get property name")?;
        return Ok(OrFalse::False);
    };
    PhpString::new(req, name.as_bytes()).map(OrFalse::Value)
}

/// `IntlChar`'s constants in `uchar.stub.php`'s order after
/// `UNICODE_VERSION`: each word a name under the last `@` prefix, its
/// value written after `=` or else one past the last int's.
const TABLE: &str = "\
    CODEPOINT_MIN=0 CODEPOINT_MAX=1114111 NO_NUMERIC_VALUE=-123456789.0 @PROPERTY_ \
    ALPHABETIC=0 BINARY_START=0 ASCII_HEX_DIGIT BIDI_CONTROL BIDI_MIRRORED DASH \
    DEFAULT_IGNORABLE_CODE_POINT DEPRECATED DIACRITIC EXTENDER FULL_COMPOSITION_EXCLUSION \
    GRAPHEME_BASE GRAPHEME_EXTEND GRAPHEME_LINK HEX_DIGIT HYPHEN ID_CONTINUE ID_START \
    IDEOGRAPHIC IDS_BINARY_OPERATOR IDS_TRINARY_OPERATOR IDS_UNARY_OPERATOR=72 \
    ID_COMPAT_MATH_START ID_COMPAT_MATH_CONTINUE JOIN_CONTROL=20 LOGICAL_ORDER_EXCEPTION \
    LOWERCASE MATH NONCHARACTER_CODE_POINT QUOTATION_MARK RADICAL SOFT_DOTTED \
    TERMINAL_PUNCTUATION UNIFIED_IDEOGRAPH UPPERCASE WHITE_SPACE XID_CONTINUE XID_START \
    CASE_SENSITIVE S_TERM VARIATION_SELECTOR NFD_INERT NFKD_INERT NFC_INERT NFKC_INERT \
    SEGMENT_STARTER PATTERN_SYNTAX PATTERN_WHITE_SPACE POSIX_ALNUM POSIX_BLANK POSIX_GRAPH \
    POSIX_PRINT POSIX_XDIGIT CASED CASE_IGNORABLE CHANGES_WHEN_LOWERCASED \
    CHANGES_WHEN_UPPERCASED CHANGES_WHEN_TITLECASED CHANGES_WHEN_CASEFOLDED \
    CHANGES_WHEN_CASEMAPPED CHANGES_WHEN_NFKC_CASEFOLDED BINARY_LIMIT=76 BIDI_CLASS=4096 \
    INT_START=4096 BLOCK CANONICAL_COMBINING_CLASS DECOMPOSITION_TYPE EAST_ASIAN_WIDTH \
    GENERAL_CATEGORY JOINING_GROUP JOINING_TYPE LINE_BREAK NUMERIC_TYPE SCRIPT \
    HANGUL_SYLLABLE_TYPE NFD_QUICK_CHECK NFKD_QUICK_CHECK NFC_QUICK_CHECK NFKC_QUICK_CHECK \
    LEAD_CANONICAL_COMBINING_CLASS TRAIL_CANONICAL_COMBINING_CLASS GRAPHEME_CLUSTER_BREAK \
    SENTENCE_BREAK WORD_BREAK BIDI_PAIRED_BRACKET_TYPE INT_LIMIT=4123 \
    GENERAL_CATEGORY_MASK=8192 MASK_START=8192 MASK_LIMIT NUMERIC_VALUE=12288 \
    DOUBLE_START=12288 DOUBLE_LIMIT AGE=16384 STRING_START=16384 BIDI_MIRRORING_GLYPH \
    CASE_FOLDING ISO_COMMENT LOWERCASE_MAPPING NAME SIMPLE_CASE_FOLDING \
    SIMPLE_LOWERCASE_MAPPING SIMPLE_TITLECASE_MAPPING SIMPLE_UPPERCASE_MAPPING \
    TITLECASE_MAPPING UNICODE_1_NAME UPPERCASE_MAPPING BIDI_PAIRED_BRACKET STRING_LIMIT \
    SCRIPT_EXTENSIONS=28672 OTHER_PROPERTY_START=28672 OTHER_PROPERTY_LIMIT=28674 \
    INVALID_CODE=-1 @CHAR_CATEGORY_ UNASSIGNED GENERAL_OTHER_TYPES=0 UPPERCASE_LETTER \
    LOWERCASE_LETTER TITLECASE_LETTER MODIFIER_LETTER OTHER_LETTER NON_SPACING_MARK \
    ENCLOSING_MARK COMBINING_SPACING_MARK DECIMAL_DIGIT_NUMBER LETTER_NUMBER OTHER_NUMBER \
    SPACE_SEPARATOR LINE_SEPARATOR PARAGRAPH_SEPARATOR CONTROL_CHAR FORMAT_CHAR \
    PRIVATE_USE_CHAR SURROGATE DASH_PUNCTUATION START_PUNCTUATION END_PUNCTUATION \
    CONNECTOR_PUNCTUATION OTHER_PUNCTUATION MATH_SYMBOL CURRENCY_SYMBOL MODIFIER_SYMBOL \
    OTHER_SYMBOL INITIAL_PUNCTUATION FINAL_PUNCTUATION CHAR_CATEGORY_COUNT @CHAR_DIRECTION_ \
    LEFT_TO_RIGHT=0 RIGHT_TO_LEFT EUROPEAN_NUMBER EUROPEAN_NUMBER_SEPARATOR \
    EUROPEAN_NUMBER_TERMINATOR ARABIC_NUMBER COMMON_NUMBER_SEPARATOR BLOCK_SEPARATOR \
    SEGMENT_SEPARATOR WHITE_SPACE_NEUTRAL OTHER_NEUTRAL LEFT_TO_RIGHT_EMBEDDING \
    LEFT_TO_RIGHT_OVERRIDE RIGHT_TO_LEFT_ARABIC RIGHT_TO_LEFT_EMBEDDING \
    RIGHT_TO_LEFT_OVERRIDE POP_DIRECTIONAL_FORMAT DIR_NON_SPACING_MARK BOUNDARY_NEUTRAL \
    FIRST_STRONG_ISOLATE LEFT_TO_RIGHT_ISOLATE RIGHT_TO_LEFT_ISOLATE POP_DIRECTIONAL_ISOLATE \
    CHAR_DIRECTION_COUNT @BLOCK_CODE_ NO_BLOCK=0 BASIC_LATIN LATIN_1_SUPPLEMENT \
    LATIN_EXTENDED_A LATIN_EXTENDED_B IPA_EXTENSIONS SPACING_MODIFIER_LETTERS \
    COMBINING_DIACRITICAL_MARKS GREEK CYRILLIC ARMENIAN HEBREW ARABIC SYRIAC THAANA \
    DEVANAGARI BENGALI GURMUKHI GUJARATI ORIYA TAMIL TELUGU KANNADA MALAYALAM SINHALA THAI \
    LAO TIBETAN MYANMAR GEORGIAN HANGUL_JAMO ETHIOPIC CHEROKEE \
    UNIFIED_CANADIAN_ABORIGINAL_SYLLABICS OGHAM RUNIC KHMER MONGOLIAN \
    LATIN_EXTENDED_ADDITIONAL GREEK_EXTENDED GENERAL_PUNCTUATION SUPERSCRIPTS_AND_SUBSCRIPTS \
    CURRENCY_SYMBOLS COMBINING_MARKS_FOR_SYMBOLS LETTERLIKE_SYMBOLS NUMBER_FORMS ARROWS \
    MATHEMATICAL_OPERATORS MISCELLANEOUS_TECHNICAL CONTROL_PICTURES \
    OPTICAL_CHARACTER_RECOGNITION ENCLOSED_ALPHANUMERICS BOX_DRAWING BLOCK_ELEMENTS \
    GEOMETRIC_SHAPES MISCELLANEOUS_SYMBOLS DINGBATS BRAILLE_PATTERNS CJK_RADICALS_SUPPLEMENT \
    KANGXI_RADICALS IDEOGRAPHIC_DESCRIPTION_CHARACTERS CJK_SYMBOLS_AND_PUNCTUATION HIRAGANA \
    KATAKANA BOPOMOFO HANGUL_COMPATIBILITY_JAMO KANBUN BOPOMOFO_EXTENDED \
    ENCLOSED_CJK_LETTERS_AND_MONTHS CJK_COMPATIBILITY CJK_UNIFIED_IDEOGRAPHS_EXTENSION_A \
    CJK_UNIFIED_IDEOGRAPHS YI_SYLLABLES YI_RADICALS HANGUL_SYLLABLES HIGH_SURROGATES \
    HIGH_PRIVATE_USE_SURROGATES LOW_SURROGATES PRIVATE_USE_AREA PRIVATE_USE=78 \
    CJK_COMPATIBILITY_IDEOGRAPHS ALPHABETIC_PRESENTATION_FORMS ARABIC_PRESENTATION_FORMS_A \
    COMBINING_HALF_MARKS CJK_COMPATIBILITY_FORMS SMALL_FORM_VARIANTS \
    ARABIC_PRESENTATION_FORMS_B SPECIALS HALFWIDTH_AND_FULLWIDTH_FORMS OLD_ITALIC GOTHIC \
    DESERET BYZANTINE_MUSICAL_SYMBOLS MUSICAL_SYMBOLS MATHEMATICAL_ALPHANUMERIC_SYMBOLS \
    CJK_UNIFIED_IDEOGRAPHS_EXTENSION_B CJK_COMPATIBILITY_IDEOGRAPHS_SUPPLEMENT TAGS \
    CYRILLIC_SUPPLEMENT CYRILLIC_SUPPLEMENTARY=97 TAGALOG HANUNOO BUHID TAGBANWA \
    MISCELLANEOUS_MATHEMATICAL_SYMBOLS_A SUPPLEMENTAL_ARROWS_A SUPPLEMENTAL_ARROWS_B \
    MISCELLANEOUS_MATHEMATICAL_SYMBOLS_B SUPPLEMENTAL_MATHEMATICAL_OPERATORS \
    KATAKANA_PHONETIC_EXTENSIONS VARIATION_SELECTORS SUPPLEMENTARY_PRIVATE_USE_AREA_A \
    SUPPLEMENTARY_PRIVATE_USE_AREA_B LIMBU TAI_LE KHMER_SYMBOLS PHONETIC_EXTENSIONS \
    MISCELLANEOUS_SYMBOLS_AND_ARROWS YIJING_HEXAGRAM_SYMBOLS LINEAR_B_SYLLABARY \
    LINEAR_B_IDEOGRAMS AEGEAN_NUMBERS UGARITIC SHAVIAN OSMANYA CYPRIOT_SYLLABARY \
    TAI_XUAN_JING_SYMBOLS VARIATION_SELECTORS_SUPPLEMENT ANCIENT_GREEK_MUSICAL_NOTATION \
    ANCIENT_GREEK_NUMBERS ARABIC_SUPPLEMENT BUGINESE CJK_STROKES \
    COMBINING_DIACRITICAL_MARKS_SUPPLEMENT COPTIC ETHIOPIC_EXTENDED ETHIOPIC_SUPPLEMENT \
    GEORGIAN_SUPPLEMENT GLAGOLITIC KHAROSHTHI MODIFIER_TONE_LETTERS NEW_TAI_LUE OLD_PERSIAN \
    PHONETIC_EXTENSIONS_SUPPLEMENT SUPPLEMENTAL_PUNCTUATION SYLOTI_NAGRI TIFINAGH \
    VERTICAL_FORMS NKO BALINESE LATIN_EXTENDED_C LATIN_EXTENDED_D PHAGS_PA PHOENICIAN \
    CUNEIFORM CUNEIFORM_NUMBERS_AND_PUNCTUATION COUNTING_ROD_NUMERALS SUNDANESE LEPCHA \
    OL_CHIKI CYRILLIC_EXTENDED_A VAI CYRILLIC_EXTENDED_B SAURASHTRA KAYAH_LI REJANG CHAM \
    ANCIENT_SYMBOLS PHAISTOS_DISC LYCIAN CARIAN LYDIAN MAHJONG_TILES DOMINO_TILES SAMARITAN \
    UNIFIED_CANADIAN_ABORIGINAL_SYLLABICS_EXTENDED TAI_THAM VEDIC_EXTENSIONS LISU BAMUM \
    COMMON_INDIC_NUMBER_FORMS DEVANAGARI_EXTENDED HANGUL_JAMO_EXTENDED_A JAVANESE \
    MYANMAR_EXTENDED_A TAI_VIET MEETEI_MAYEK HANGUL_JAMO_EXTENDED_B IMPERIAL_ARAMAIC \
    OLD_SOUTH_ARABIAN AVESTAN INSCRIPTIONAL_PARTHIAN INSCRIPTIONAL_PAHLAVI OLD_TURKIC \
    RUMI_NUMERAL_SYMBOLS KAITHI EGYPTIAN_HIEROGLYPHS ENCLOSED_ALPHANUMERIC_SUPPLEMENT \
    ENCLOSED_IDEOGRAPHIC_SUPPLEMENT CJK_UNIFIED_IDEOGRAPHS_EXTENSION_C MANDAIC BATAK \
    ETHIOPIC_EXTENDED_A BRAHMI BAMUM_SUPPLEMENT KANA_SUPPLEMENT PLAYING_CARDS \
    MISCELLANEOUS_SYMBOLS_AND_PICTOGRAPHS EMOTICONS TRANSPORT_AND_MAP_SYMBOLS \
    ALCHEMICAL_SYMBOLS CJK_UNIFIED_IDEOGRAPHS_EXTENSION_D ARABIC_EXTENDED_A \
    ARABIC_MATHEMATICAL_ALPHABETIC_SYMBOLS CHAKMA MEETEI_MAYEK_EXTENSIONS MEROITIC_CURSIVE \
    MEROITIC_HIEROGLYPHS MIAO SHARADA SORA_SOMPENG SUNDANESE_SUPPLEMENT TAKRI BASSA_VAH \
    CAUCASIAN_ALBANIAN COPTIC_EPACT_NUMBERS COMBINING_DIACRITICAL_MARKS_EXTENDED DUPLOYAN \
    ELBASAN GEOMETRIC_SHAPES_EXTENDED GRANTHA KHOJKI KHUDAWADI LATIN_EXTENDED_E LINEAR_A \
    MAHAJANI MANICHAEAN MENDE_KIKAKUI MODI MRO MYANMAR_EXTENDED_B NABATAEAN \
    OLD_NORTH_ARABIAN OLD_PERMIC ORNAMENTAL_DINGBATS PAHAWH_HMONG PALMYRENE PAU_CIN_HAU \
    PSALTER_PAHLAVI SHORTHAND_FORMAT_CONTROLS SIDDHAM SINHALA_ARCHAIC_NUMBERS \
    SUPPLEMENTAL_ARROWS_C TIRHUTA WARANG_CITI COUNT=354 INVALID_CODE=-1 @BPT_ NONE OPEN \
    CLOSE COUNT @EA_ NEUTRAL=0 AMBIGUOUS HALFWIDTH FULLWIDTH NARROW WIDE COUNT @ \
    UNICODE_CHAR_NAME=0 UNICODE_10_CHAR_NAME EXTENDED_CHAR_NAME CHAR_NAME_ALIAS \
    CHAR_NAME_CHOICE_COUNT SHORT_PROPERTY_NAME=0 LONG_PROPERTY_NAME @PROPERTY_ \
    NAME_CHOICE_COUNT @DT_ NONE=0 CANONICAL COMPAT CIRCLE FINAL FONT FRACTION INITIAL \
    ISOLATED MEDIAL NARROW NOBREAK SMALL SQUARE SUB SUPER VERTICAL WIDE COUNT @JT_ \
    NON_JOINING=0 JOIN_CAUSING DUAL_JOINING LEFT_JOINING RIGHT_JOINING TRANSPARENT COUNT \
    @JG_ NO_JOINING_GROUP=0 AIN ALAPH ALEF BEH BETH DAL DALATH_RISH E FEH FINAL_SEMKATH GAF \
    GAMAL HAH TEH_MARBUTA_GOAL HAMZA_ON_HEH_GOAL=14 HE HEH HEH_GOAL HETH KAF KAPH \
    KNOTTED_HEH LAM LAMADH MEEM MIM NOON NUN PE QAF QAPH REH REVERSED_PE SAD SADHE SEEN \
    SEMKATH SHIN SWASH_KAF SYRIAC_WAW TAH TAW TEH_MARBUTA TETH WAW YEH YEH_BARREE \
    YEH_WITH_TAIL YUDH YUDH_HE ZAIN FE KHAPH ZHAIN BURUSHASKI_YEH_BARREE FARSI_YEH NYA \
    ROHINGYA_YEH MANICHAEAN_ALEPH MANICHAEAN_AYIN MANICHAEAN_BETH MANICHAEAN_DALETH \
    MANICHAEAN_DHAMEDH MANICHAEAN_FIVE MANICHAEAN_GIMEL MANICHAEAN_HETH MANICHAEAN_HUNDRED \
    MANICHAEAN_KAPH MANICHAEAN_LAMEDH MANICHAEAN_MEM MANICHAEAN_NUN MANICHAEAN_ONE \
    MANICHAEAN_PE MANICHAEAN_QOPH MANICHAEAN_RESH MANICHAEAN_SADHE MANICHAEAN_SAMEKH \
    MANICHAEAN_TAW MANICHAEAN_TEN MANICHAEAN_TETH MANICHAEAN_THAMEDH MANICHAEAN_TWENTY \
    MANICHAEAN_WAW MANICHAEAN_YODH MANICHAEAN_ZAYIN STRAIGHT_WAW COUNT=116 @GCB_ OTHER=0 \
    CONTROL CR EXTEND L LF LV LVT T V SPACING_MARK PREPEND REGIONAL_INDICATOR COUNT=18 @WB_ \
    OTHER=0 ALETTER FORMAT KATAKANA MIDLETTER MIDNUM NUMERIC EXTENDNUMLET CR EXTEND LF \
    MIDNUMLET NEWLINE REGIONAL_INDICATOR HEBREW_LETTER SINGLE_QUOTE DOUBLE_QUOTE COUNT=23 \
    @SB_ OTHER=0 ATERM CLOSE FORMAT LOWER NUMERIC OLETTER SEP SP STERM UPPER CR EXTEND LF \
    SCONTINUE COUNT @LB_ UNKNOWN=0 AMBIGUOUS ALPHABETIC BREAK_BOTH BREAK_AFTER BREAK_BEFORE \
    MANDATORY_BREAK CONTINGENT_BREAK CLOSE_PUNCTUATION COMBINING_MARK CARRIAGE_RETURN \
    EXCLAMATION GLUE HYPHEN IDEOGRAPHIC INSEPARABLE INSEPERABLE=15 INFIX_NUMERIC LINE_FEED \
    NONSTARTER NUMERIC OPEN_PUNCTUATION POSTFIX_NUMERIC PREFIX_NUMERIC QUOTATION \
    COMPLEX_CONTEXT SURROGATE SPACE BREAK_SYMBOLS ZWSPACE NEXT_LINE WORD_JOINER H2 H3 JL JT \
    JV CLOSE_PARENTHESIS CONDITIONAL_JAPANESE_STARTER HEBREW_LETTER REGIONAL_INDICATOR \
    COUNT=49 @NT_ NONE=0 DECIMAL DIGIT NUMERIC COUNT @HST_ NOT_APPLICABLE=0 LEADING_JAMO \
    VOWEL_JAMO TRAILING_JAMO LV_SYLLABLE LVT_SYLLABLE COUNT @ FOLD_CASE_DEFAULT=0 \
    FOLD_CASE_EXCLUDE_SPECIAL_I";

/// `register_class_IntlChar`'s constants: `UNICODE_VERSION`, then
/// [`TABLE`] read out, `NO_NUMERIC_VALUE` its one float. The list is
/// laid at the length it ends at, one row per word that is not a
/// prefix, and each name at its own.
pub(crate) fn constants() -> Vec<ClassConstDecl> {
    let words = TABLE.split_ascii_whitespace();
    let count = words.clone().filter(|word| !word.starts_with('@')).count();
    let mut constants = Vec::with_capacity(1 + count);
    constants.push(constant(
        Name(b"UNICODE_VERSION"[..].into()),
        &ffi::unicode_version(),
    ));
    let (mut prefix, mut next) = ("", 0i64);
    for word in words {
        if let Some(rest) = word.strip_prefix('@') {
            prefix = rest;
            continue;
        }
        let (name, value) = word.split_once('=').unwrap_or((word, ""));
        let name = Name([prefix.as_bytes(), name.as_bytes()].concat().into());
        if value.contains('.') {
            constants.push(constant(name, &value.parse::<f64>().unwrap_or(0.0)));
            continue;
        }
        next = value.parse().unwrap_or(next);
        constants.push(constant(name, &next));
        next += 1;
    }
    constants
}

/// One public typed constant.
fn constant<T: IntoConst>(name: Name, value: &T) -> ClassConstDecl {
    ClassConstDecl {
        name,
        visibility: Visibility::Public,
        is_final: false,
        is_case: false,
        ty: T::DECLARED,
        value: value.value(),
        attributes: Vec::new().into(),
        doc_comment: None,
    }
}

#[cfg(test)]
mod tests {
    /// The list is laid at the length it ends at, and each name at its
    /// own: nothing is grown and given back as it is read out.
    #[test]
    fn constants_are_laid_at_their_length() {
        let constants = super::constants();
        assert_eq!(constants.capacity(), constants.len());
        let words = super::TABLE.split_ascii_whitespace();
        assert_eq!(
            constants.len(),
            1 + words.filter(|w| !w.starts_with('@')).count()
        );
        let names = constants.iter().map(|c| &*c.name.0);
        let names: Vec<&[u8]> = names.skip(3).take(2).collect();
        assert_eq!(names, [&b"NO_NUMERIC_VALUE"[..], b"PROPERTY_ALPHABETIC"]);
    }
}
