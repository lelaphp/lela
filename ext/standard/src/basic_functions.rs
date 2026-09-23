//! `Z/ext/standard/basic_functions.c`'s environment and ini natives:
//! `getenv`, `putenv`, `time_nanosleep`, the include path's pair,
//! `ip2long` and `long2ip`, and `parse_ini_string` and `parse_ini_file`
//! over the INI grammar of [`crate::ini`], and the stored diagnostic's
//! pair, `error_get_last` and `error_clear_last`.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::decl::{ConstDecl, FunctionDecl};
use php_ext::raising::{Thrown, arg_value_error, throw, warning};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value};

use crate::ini::{SCANNER_NORMAL, SCANNER_RAW, SCANNER_TYPED};
use crate::md5::path_argument;
use crate::output::Entry;
use crate::stub::stub;
use crate::zpp::{given, nullable_string, optional_bool, optional_long, string_value};
use crate::{argument_count_between, bool_argument, constant, long_argument, string_argument};

/// The natives, in the stub's order.
///
/// ```php
/// echo new ReflectionFunction('getenv'), new ReflectionFunction('parse_ini_file');
/// ```
/// ```expect
/// Function [ <internal:standard> function getenv ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <optional> ?string $name = null ]
///     Parameter #1 [ <optional> bool $local_only = false ]
///   }
///   - Return [ array|string|false ]
/// }
/// Function [ <internal:standard> function parse_ini_file ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $filename ]
///     Parameter #1 [ <optional> bool $process_sections = false ]
///     Parameter #2 [ <optional> int $scanner_mode = INI_SCANNER_NORMAL ]
///   }
///   - Return [ array|false ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (stub("ip2long(string $ip): int|false"), ip2long as Entry),
        (stub("long2ip(int $ip): string"), long2ip),
        (
            stub("getenv(?string $name = null, bool $local_only = false): array|string|false"),
            getenv,
        ),
        (stub("putenv(string $assignment): bool"), putenv),
        (
            stub("time_nanosleep(int $seconds, int $nanoseconds): array|bool"),
            time_nanosleep,
        ),
        (stub("error_get_last(): ?array"), error_get_last),
        (stub("error_clear_last(): void"), error_clear_last),
        (
            stub("set_include_path(string $include_path): string|false"),
            set_include_path,
        ),
        (stub("get_include_path(): string|false"), get_include_path),
        (
            stub(
                "parse_ini_file(string $filename, bool $process_sections = false, int $scanner_mode = INI_SCANNER_NORMAL): array|false",
            ),
            parse_ini_file,
        ),
        (
            stub(
                "parse_ini_string(string $ini_string, bool $process_sections = false, int $scanner_mode = INI_SCANNER_NORMAL): array|false",
            ),
            parse_ini_string,
        ),
    ]
}

/// `INI_SCANNER_NORMAL`, `INI_SCANNER_RAW` and `INI_SCANNER_TYPED`.
///
/// ```php
/// var_dump(INI_SCANNER_NORMAL, INI_SCANNER_RAW, INI_SCANNER_TYPED);
/// ```
/// ```expect
/// int(0)
/// int(1)
/// int(2)
/// ```
pub(crate) fn constants() -> Vec<ConstDecl> {
    vec![
        constant(b"INI_SCANNER_NORMAL", SCANNER_NORMAL),
        constant(b"INI_SCANNER_RAW", SCANNER_RAW),
        constant(b"INI_SCANNER_TYPED", SCANNER_TYPED),
    ]
}

/// The four bytes of a dotted quad, as the host's `inet_pton(AF_INET)`
/// reads one: the BSD `inet_pton4`, four decimal parts of at most 255
/// each, a leading zero allowed.
fn inet_pton4(text: &[u8]) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut at = 0;
    let mut octets = 0;
    let mut saw_digit = false;
    for &c in text {
        if c.is_ascii_digit() {
            let next = u32::from(out[at]) * 10 + u32::from(c - b'0');
            out[at] = u8::try_from(next).ok()?;
            if !saw_digit {
                octets += 1;
                if octets > 4 {
                    return None;
                }
                saw_digit = true;
            }
        } else if c == b'.' && saw_digit {
            if octets == 4 {
                return None;
            }
            at += 1;
            saw_digit = false;
        } else {
            return None;
        }
    }
    (octets == 4).then_some(out)
}

/// `ip2long(string $ip): int|false`: the dotted quad as a number, or
/// `false` for text the host's `inet_pton` refuses.
///
/// ```php
/// var_dump(ip2long("1.2.3.4"), ip2long("255.255.255.255"), ip2long("01.2.3.4"), ip2long("1.2.3"), ip2long("256.1.1.1"), ip2long(""), ip2long(" 1.2.3.4"), ip2long("1.2.3.4."));
/// try { ip2long("1.2.3.4\0"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(16909060)
/// int(4294967295)
/// int(16909060)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ip2long(): Argument #1 ($ip) must not contain any null bytes
/// ```
fn ip2long(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let ip = path_argument(req, 1)?;
    match inet_pton4(ip.as_bytes()) {
        Some(bytes) if !ip.is_empty() => Ok(Value::int(i64::from(u32::from_be_bytes(bytes)))),
        _ => Ok(Value::bool(false)),
    }
}

/// `long2ip(int $ip): string`: the low 32 bits in dotted form.
///
/// ```php
/// var_dump(long2ip(16909060), long2ip(-1), long2ip(0), long2ip(4294967296 + 5), long2ip("167772161"));
/// ```
/// ```expect
/// string(7) "1.2.3.4"
/// string(15) "255.255.255.255"
/// string(7) "0.0.0.0"
/// string(7) "0.0.0.5"
/// string(8) "10.0.0.1"
/// ```
fn long2ip(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let ip = long_argument(req, 1)?;
    let [a, b, c, d] = u32::try_from(ip & 0xFFFF_FFFF).unwrap_or(0).to_be_bytes();
    string_value(req, format!("{a}.{b}.{c}.{d}").as_bytes())
}

/// `getenv(?string $name = null, bool $local_only = false):
/// array|string|false`: the variable, `false` where it is not set, and
/// with no name every variable whose name holds no space, dot or
/// bracket, as `import_environment_variable` keeps them.
///
/// ```php
/// putenv("DAX_T=on"); putenv("DAX_U=");
/// var_dump(getenv("DAX_T"), getenv("DAX_U"), getenv("NOT_SET_HERE"), getenv("DAX_T", true));
/// $all = getenv();
/// var_dump(is_array($all), $all["DAX_T"], $all["DAX_U"]);
/// ```
/// ```expect
/// string(2) "on"
/// string(0) ""
/// bool(false)
/// string(2) "on"
/// bool(true)
/// string(2) "on"
/// string(0) ""
/// ```
fn getenv(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 2)?;
    let name = nullable_string(req, 1)?;
    if given(req, 2).is_some() {
        bool_argument(req, 2)?;
    }
    let Some(name) = name else {
        let pairs = req.env_pairs().to_vec();
        let mut array = PhpArray::new(req)?;
        for (name, value) in pairs {
            if name.is_empty() || name.iter().any(|c| matches!(c, b' ' | b'.' | b'[')) {
                continue;
            }
            let key = Key::from_string(PhpString::new(req, &name)?);
            let value = string_value(req, &value)?;
            array.set(req, key, value)?;
        }
        return Ok(Value::array(array));
    };
    let name = c_string(name.as_bytes());
    match req.env(name).map(<[u8]>::to_vec) {
        Some(value) => string_value(req, &value),
        None => Ok(Value::bool(false)),
    }
}

/// The bytes before the first NUL, what a C string holds.
fn c_string(bytes: &[u8]) -> &[u8] {
    bytes.split(|c| *c == 0).next().unwrap_or_default()
}

/// `putenv(string $assignment): bool`: `NAME=value` sets the variable
/// and `NAME` alone unsets it.
///
/// ```php
/// var_dump(putenv("DAX_A=one"), getenv("DAX_A"), putenv("DAX_A=two=2"), getenv("DAX_A"), putenv("DAX_A"), getenv("DAX_A"), putenv("DAX_B="), getenv("DAX_B"));
/// try { putenv("=x"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { putenv(""); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(3) "one"
/// bool(true)
/// string(5) "two=2"
/// bool(true)
/// bool(false)
/// bool(true)
/// string(0) ""
/// putenv(): Argument #1 ($assignment) must have a valid syntax
/// putenv(): Argument #1 ($assignment) must have a valid syntax
/// ```
fn putenv(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let setting = string_argument(req, 1)?;
    let bytes = setting.as_bytes();
    if bytes.first().is_none_or(|c| *c == b'=') {
        return Err(arg_value_error(req, 1, "have a valid syntax"));
    }
    let text = c_string(bytes).to_vec();
    match text.iter().position(|c| *c == b'=') {
        Some(at) => req.set_env(&text[..at], Some(&text[at + 1..])),
        None => req.set_env(&text, None),
    };
    Ok(Value::bool(true))
}

/// `time_nanosleep(int $seconds, int $nanoseconds): array|bool`: `true`
/// after the sleep; a count of nanoseconds past 999 999 999 is the
/// `EINVAL` php turns into its `ValueError`.
///
/// ```php
/// var_dump(time_nanosleep(0, 1000));
/// foreach ([[-1, 0], [0, -1], [0, 1000000000]] as $a) {
///     try { time_nanosleep($a[0], $a[1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// bool(true)
/// time_nanosleep(): Argument #1 ($seconds) must be greater than or equal to 0
/// time_nanosleep(): Argument #2 ($nanoseconds) must be greater than or equal to 0
/// Nanoseconds was not in the range 0 to 999 999 999 or seconds was negative
/// ```
fn time_nanosleep(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let seconds = long_argument(req, 1)?;
    let nanoseconds = long_argument(req, 2)?;
    let Ok(seconds) = u64::try_from(seconds) else {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    };
    let Ok(nanoseconds) = u32::try_from(nanoseconds) else {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    };
    if nanoseconds > 999_999_999 {
        return Err(throw(
            req,
            "ValueError",
            "Nanoseconds was not in the range 0 to 999 999 999 or seconds was negative",
        ));
    }
    std::thread::sleep(std::time::Duration::new(seconds, nanoseconds));
    Ok(Value::bool(true))
}

/// `set_include_path(string $include_path): string|false`: the old
/// path, the new one in place; `false` for an empty one, which
/// `OnUpdateStringUnempty` refuses.
///
/// ```php
/// set_include_path("/one:/two");
/// var_dump(get_include_path(), set_include_path("/three"), get_include_path(), set_include_path(""), get_include_path(), ini_get("include_path"));
/// ```
/// ```expect
/// string(9) "/one:/two"
/// string(9) "/one:/two"
/// string(6) "/three"
/// bool(false)
/// string(6) "/three"
/// string(6) "/three"
/// ```
fn set_include_path(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let path = path_argument(req, 1)?;
    let Some(old) = req.ini_get(b"include_path").map(<[u8]>::to_vec) else {
        return Ok(Value::bool(false));
    };
    if path.is_empty() || req.ini_set(b"include_path", path.as_bytes())?.is_none() {
        return Ok(Value::bool(false));
    }
    string_value(req, &old)
}

/// `get_include_path(): string|false`: the `include_path` entry.
///
/// ```php
/// ini_set("include_path", "/a");
/// var_dump(get_include_path());
/// try { get_include_path(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(2) "/a"
/// get_include_path() expects exactly 0 arguments, 1 given
/// ```
fn get_include_path(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    match req.ini_get(b"include_path").map(<[u8]>::to_vec) {
        Some(path) => string_value(req, &path),
        None => Ok(Value::bool(false)),
    }
}

/// `error_get_last(): ?array`, php's `PHP_FUNCTION(error_get_last)`
/// (`Z/ext/standard/basic_functions.c:1420`): the last diagnostic the
/// built-in handler stored, as `type`, `message`, `file` and `line`, and
/// `null` while none is. The handler stores what `error_reporting` and
/// `@` hide, and nothing a user handler served; a handler that answers
/// `false` falls through to it and the diagnostic is stored.
///
/// ```php
/// var_dump(error_get_last()); @trigger_error("x", E_USER_WARNING); var_dump(error_get_last()); var_dump(error_clear_last(), error_get_last());
/// set_error_handler(fn () => true); trigger_error("y"); var_dump(error_get_last());
/// set_error_handler(fn () => false); trigger_error("z"); var_dump(error_get_last()["message"]);
/// ```
/// ```expect
/// NULL
/// array(4) {
///   ["type"]=>
///   int(512)
///   ["message"]=>
///   string(1) "x"
///   ["file"]=>
///   string(%d) "%s"
///   ["line"]=>
///   int(2)
/// }
/// NULL
/// NULL
/// NULL
///
/// Notice: z in %s on line 4
/// string(1) "z"
/// ```
fn error_get_last(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    Ok(req.last_error()?.map_or_else(Value::null, Value::array))
}

/// `error_clear_last(): void`, php's `PHP_FUNCTION(error_clear_last)`
/// (`Z/ext/standard/basic_functions.c:1449`): the stored diagnostic
/// dropped, so `error_get_last()` answers `null` until the next.
///
/// ```php
/// @trigger_error("x"); error_clear_last(); var_dump(error_get_last()); error_clear_last(1);
/// ```
/// ```expect exit=255
/// NULL
///
/// Fatal error: Uncaught ArgumentCountError: error_clear_last() expects exactly 0 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): error_clear_last(1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn error_clear_last(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    req.clear_last_error();
    Ok(Value::null())
}

/// The two optional arguments of the pair, `$process_sections` and
/// `$scanner_mode`.
fn ini_options(req: &mut Request) -> Result<(bool, i64), Thrown> {
    let sections = optional_bool(req, 2, false)?;
    let mode = optional_long(req, 3, SCANNER_NORMAL)?;
    Ok((sections, mode))
}

/// `zend_parse_ini_string`/`zend_parse_ini_file` and the callback's
/// array: `false` after the warning for a `$scanner_mode` that is none of
/// the three or for a syntax error, which `ini_error` writes with the
/// file's name, `Unknown` for a string, and the line the scanner was on.
fn parse_ini(
    req: &mut Request,
    bytes: &[u8],
    file: &[u8],
    sections: bool,
    mode: i64,
) -> Result<Value, Thrown> {
    if !matches!(mode, SCANNER_NORMAL | SCANNER_RAW | SCANNER_TYPED) {
        warning(req, "Invalid scanner mode");
        return Ok(Value::bool(false));
    }
    match crate::ini::parse(req, bytes, mode)? {
        Ok(events) => crate::ini::assemble(req, events, sections),
        Err((message, line)) => {
            let file = String::from_utf8_lossy(file);
            warning(req, &format!("{message} in {file} on line {line}\n"));
            Ok(Value::bool(false))
        }
    }
}

/// The string's entries, the string read to its first NUL, as
/// `zend_ini_prepare_string_for_scanning` takes its `strlen`:
/// `parse_ini_string(string $ini_string, bool $process_sections = false, int $scanner_mode = INI_SCANNER_NORMAL): array|false`.
///
/// ```php
/// putenv("DAX_T=t");
/// var_dump(parse_ini_string("a = 1\nb = on\nc = \"q \\\"x\\\" \${DAX_T}\"\nd[] = 1\nd[] = 2\nd[k] = v\n5 = five\n; note\n[sec]\ne = E_ALL & ~E_NOTICE\nf = null"));
/// ```
/// ```expect
/// array(7) {
///   ["a"]=>
///   string(1) "1"
///   ["b"]=>
///   string(1) "1"
///   ["c"]=>
///   string(7) "q "x" t"
///   ["d"]=>
///   array(3) {
///     [0]=>
///     string(1) "1"
///     [1]=>
///     string(1) "2"
///     ["k"]=>
///     string(1) "v"
///   }
///   [5]=>
///   string(4) "five"
///   ["e"]=>
///   string(5) "30711"
///   ["f"]=>
///   string(0) ""
/// }
/// ```
///
/// Sections nest their entries, and the typed scanner answers booleans,
/// null and the numbers that were written as numbers.
///
/// ```php
/// var_dump(parse_ini_string("top = 1\n[one]\nx = yes\ny = 1.5\nz = 0x1A\n[two]\nw = -3\nv = \"7\"\nu = 99999999999999999999", true, INI_SCANNER_TYPED));
/// ```
/// ```expect
/// array(3) {
///   ["top"]=>
///   int(1)
///   ["one"]=>
///   array(3) {
///     ["x"]=>
///     bool(true)
///     ["y"]=>
///     float(1.5)
///     ["z"]=>
///     string(4) "0x1A"
///   }
///   ["two"]=>
///   array(3) {
///     ["w"]=>
///     int(-3)
///     ["v"]=>
///     string(1) "7"
///     ["u"]=>
///     string(20) "99999999999999999999"
///   }
/// }
/// ```
///
/// The raw scanner leaves values as written, quotes around the whole
/// value taken off and a comment cut at its first `;`.
///
/// ```php
/// var_dump(parse_ini_string("a = \"x;y\" ; c\nb = on ; c\nc = \${X} | 1\n[s ~ t]\nd = 'q'", true, INI_SCANNER_RAW));
/// ```
/// ```expect
/// array(4) {
///   ["a"]=>
///   string(3) "x;y"
///   ["b"]=>
///   string(2) "on"
///   ["c"]=>
///   string(8) "${X} | 1"
///   ["s ~ t"]=>
///   array(1) {
///     ["d"]=>
///     string(3) "'q'"
///   }
/// }
/// ```
///
/// A syntax error is a warning and `false`; so is a `$scanner_mode`
/// that is none of the three.
///
/// ```php
/// var_dump(parse_ini_string("a = (1"), parse_ini_string("x\ny = 1\nz = 1 |"), parse_ini_string("a = \${x"), parse_ini_string("[a"), parse_ini_string("a=\"b"), parse_ini_string("a[x]"), parse_ini_string("a", false, 7));
/// ```
/// ```expect
/// Warning: syntax error, unexpected END_OF_LINE, expecting '^' or '|' or '&' or ')' in Unknown on line 1
///  in %s on line 2
///
/// Warning: syntax error, unexpected END_OF_LINE in Unknown on line 3
///  in %s on line 2
///
/// Warning: syntax error, unexpected end of file, expecting TC_FALLBACK or '}' in Unknown on line 1
///  in %s on line 2
///
/// Warning: syntax error, unexpected end of file, expecting ']' in Unknown on line 1
///  in %s on line 2
///
/// Warning: syntax error, unexpected end of file, expecting TC_DOLLAR_CURLY or TC_QUOTED_STRING or '"' in Unknown on line 1
///  in %s on line 2
///
/// Warning: syntax error, unexpected end of file, expecting '=' in Unknown on line 1
///  in %s on line 2
///
/// Warning: Invalid scanner mode in %s on line 2
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn parse_ini_string(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 3)?;
    let text = string_argument(req, 1)?;
    let (sections, mode) = ini_options(req)?;
    let bytes = c_string(text.as_bytes()).to_vec();
    parse_ini(req, &bytes, b"Unknown", sections, mode)
}

/// The file's entries, the file opened on the include path as
/// `zend_stream_open` opens it, then read as `parse_ini_string` reads a
/// string, a syntax error naming the file:
/// `parse_ini_file(string $filename, bool $process_sections = false, int $scanner_mode = INI_SCANNER_NORMAL): array|false`.
///
/// ```php
/// $f = fopen("t.ini", "w"); fwrite($f, "[a]\nx = 1\ny = \"two\"\n[b]\nz[] = 3\n"); fclose($f);
/// var_dump(parse_ini_file("t.ini", true));
/// $f = fopen("u.ini", "w"); fwrite($f, "k = v\n\nbad = (\n"); fclose($f);
/// var_dump(parse_ini_file("u.ini"), parse_ini_file("missing.ini"));
/// try { parse_ini_file(""); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   ["a"]=>
///   array(2) {
///     ["x"]=>
///     string(1) "1"
///     ["y"]=>
///     string(3) "two"
///   }
///   ["b"]=>
///   array(1) {
///     ["z"]=>
///     array(1) {
///       [0]=>
///       string(1) "3"
///     }
///   }
/// }
///
/// Warning: syntax error, unexpected END_OF_LINE in u.ini on line 4
///  in %s on line 5
///
/// Warning: parse_ini_file(missing.ini): Failed to open stream: No such file or directory in %s on line 5
/// bool(false)
/// bool(false)
/// parse_ini_file(): Argument #1 ($filename) must not be empty
/// ```
fn parse_ini_file(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 3)?;
    let filename = path_argument(req, 1)?;
    let (sections, mode) = ini_options(req)?;
    if filename.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    let Some(stream) = req.open_stream(filename.as_bytes(), b"rb", true)? else {
        return Ok(Value::bool(false));
    };
    let mut bytes = Vec::new();
    loop {
        match stream.read(req, 8192)? {
            Some(chunk) if !chunk.is_empty() => bytes.extend_from_slice(chunk.as_bytes()),
            _ => break,
        }
    }
    stream.close(req);
    let name = filename.as_bytes().to_vec();
    parse_ini(req, &bytes, &name, sections, mode)
}
