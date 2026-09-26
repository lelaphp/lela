//! `Z/ext/standard/basic_functions.c`'s environment and ini natives:
//! `getenv`, `putenv`, `time_nanosleep`, the include path's pair,
//! `ip2long` and `long2ip`, and `parse_ini_string` and `parse_ini_file`
//! over the INI grammar of [`crate::ini`], and the stored diagnostic's
//! pair, `error_get_last` and `error_clear_last`.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::os::unix::ffi::OsStrExt;

use php_ext::coerce::{numeric_string, to_string};
use php_ext::decl::{ConstDecl, FunctionDecl};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw, warning};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};

use crate::ini::{SCANNER_NORMAL, SCANNER_RAW, SCANNER_TYPED};
use crate::md5::path_argument;
use crate::output::Entry;
use crate::stub::stub;
use crate::zpp::{
    assign_ref, double_argument, given, nullable_string, optional_bool, optional_long, string_value,
};
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
            stub(
                "getopt(string $short_options, array $long_options = [], &$rest_index = null): array|false",
            ),
            getopt,
        ),
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
            stub("getservbyname(string $service, string $protocol): int|false"),
            getservbyname,
        ),
        (
            stub("getservbyport(int $port, string $protocol): string|false"),
            getservbyport,
        ),
        (
            stub("getprotobyname(string $protocol): int|false"),
            getprotobyname,
        ),
        (
            stub("getprotobynumber(int $protocol): string|false"),
            getprotobynumber,
        ),
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
    .into_iter()
    .chain(host_natives())
    .collect()
}

/// The natives over the host and the engine's tables, in the stub's
/// order.
fn host_natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (stub("flush(): void"), flush as Entry),
        (
            stub("highlight_file(string $filename, bool $return = false): string|bool"),
            highlight_file,
        ),
        (
            stub("show_source(string $filename, bool $return = false): string|bool"),
            highlight_file,
        ),
        (
            stub("php_strip_whitespace(string $filename): string"),
            php_strip_whitespace,
        ),
        (
            stub("highlight_string(string $string, bool $return = false): string|true"),
            highlight_string,
        ),
        (stub("usleep(int $microseconds): void"), usleep),
        (
            stub("time_sleep_until(float $timestamp): bool"),
            time_sleep_until,
        ),
        (stub("get_current_user(): string"), get_current_user),
        (
            stub("get_cfg_var(string $option): string|array|false"),
            get_cfg_var,
        ),
        (
            stub(
                "error_log(string $message, int $message_type = 0, ?string $destination = null, ?string $additional_headers = null): bool",
            ),
            error_log,
        ),
        (
            stub("ini_get_all(?string $extension = null, bool $details = true): array|false"),
            ini_get_all,
        ),
        (stub("ini_restore(string $option): void"), ini_restore),
        (
            stub("ini_parse_quantity(string $shorthand): int"),
            ini_parse_quantity,
        ),
        (stub("connection_aborted(): int"), connection_aborted),
        (stub("connection_status(): int"), connection_status),
        (
            stub("ignore_user_abort(?bool $enable = null): int"),
            ignore_user_abort,
        ),
        (
            stub("register_tick_function(callable $callback, mixed ...$args): bool"),
            register_tick_function,
        ),
        (
            stub("unregister_tick_function(callable $callback): void"),
            unregister_tick_function,
        ),
        (
            stub("is_uploaded_file(string $filename): bool"),
            is_uploaded_file,
        ),
        (
            stub("move_uploaded_file(string $from, string $to): bool"),
            move_uploaded_file,
        ),
        (stub("sys_getloadavg(): array|false"), sys_getloadavg),
        (
            stub("ftok(string $filename, string $project_id): int"),
            ftok,
        ),
        (stub("inet_ntop(string $ip): string|false"), inet_ntop),
        (stub("inet_pton(string $ip): string|false"), inet_pton),
        (
            stub("header_register_callback(callable $callback): bool"),
            header_register_callback,
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
        constant(b"CONNECTION_ABORTED", CONNECTION_ABORTED),
        constant(b"CONNECTION_NORMAL", CONNECTION_NORMAL),
        constant(b"CONNECTION_TIMEOUT", CONNECTION_TIMEOUT),
        constant(b"INI_USER", INI_USER),
        constant(b"INI_PERDIR", INI_PERDIR),
        constant(b"INI_SYSTEM", INI_SYSTEM),
        constant(b"INI_ALL", INI_USER | INI_PERDIR | INI_SYSTEM),
        constant(b"INI_SCANNER_NORMAL", SCANNER_NORMAL),
        constant(b"INI_SCANNER_RAW", SCANNER_RAW),
        constant(b"INI_SCANNER_TYPED", SCANNER_TYPED),
    ]
}

/// `PHP_CONNECTION_NORMAL`, `PHP_CONNECTION_ABORTED` and
/// `PHP_CONNECTION_TIMEOUT` (`Z/main/php.h`), the bits of
/// `PG(connection_status)`.
const CONNECTION_NORMAL: i64 = 0;
const CONNECTION_ABORTED: i64 = 1;
const CONNECTION_TIMEOUT: i64 = 2;

/// `ZEND_INI_USER`, `ZEND_INI_PERDIR` and `ZEND_INI_SYSTEM`
/// (`Z/Zend/zend_ini.h`), an entry's `modifiable` bits.
const INI_USER: i64 = 1;
const INI_PERDIR: i64 = 2;
const INI_SYSTEM: i64 = 4;

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

/// One option `getopt` found: its name and each value it was given.
type Found = (Vec<u8>, Vec<Option<Vec<u8>>>);

/// `getopt(string $short_options, array $long_options = [], &$rest_index
/// = null): array|false`, php's `PHP_FUNCTION(getopt)`: the options
/// `php_getopt` finds in the host's `argv` ([`crate::getopt`]), each
/// under its letter or name, an integer key where the name is one
/// without a leading zero, as its value or `false`, and as a list where
/// it was given more than once; `$rest_index` is where the first
/// argument after them sits. A request with no `argv`, a web host's,
/// answers `false`. Under the witness the `argv` is the file served alone.
///
/// ```php
/// var_dump(getopt("ab:c::", ["long:", "flag"], $rest), $rest);
/// try { getopt("a", null); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(0) {
/// }
/// int(1)
/// getopt(): Argument #2 ($long_options) must be of type array, null given
/// ```
fn getopt(req: &mut Request) -> Result<Value, Thrown> {
    use crate::getopt::{Getopt, Got, Named, long_option, short_options};
    argument_count_between(req, 1, 3)?;
    let spec = string_argument(req, 1)?;
    let long = match given(req, 2) {
        Some(_) => Some(crate::zpp::array_argument(req, 2)?),
        None => None,
    };
    let rest_index = given(req, 3).is_some();
    if rest_index {
        assign_ref(req, 3, Value::int(1))?;
    }
    // `$_SERVER['argv']` as `php_build_argv` wrote it, each argument a C
    // string; the one place `getopt` reads the host's `argv`.
    let argv: Vec<Vec<u8>> = req.argv().iter().map(|a| c_string(a).to_vec()).collect();
    if argv.is_empty() {
        return Ok(Value::bool(false));
    }
    let mut opts = short_options(c_string(spec.as_bytes()));
    if let Some(long) = long {
        for (_, value) in long.snapshot() {
            let text = to_string(req, &value)?;
            opts.push(long_option(c_string(text.as_bytes())));
            req.release(Value::string(text));
        }
    }
    let mut walk = Getopt::new(&argv, &opts);
    // Each option found, by name in the order first found, with each
    // value it was given.
    let mut found: Vec<Found> = Vec::new();
    loop {
        match walk.next() {
            Got::End => break,
            Got::Invalid => {}
            Got::Found(at, value) => {
                let name = match &opts[at].named {
                    Named::Short(letter) => vec![*letter],
                    Named::Long(name) => name.clone(),
                };
                match found.iter_mut().find(|(n, _)| *n == name) {
                    Some((_, values)) => values.push(value),
                    None => found.push((name, vec![value])),
                }
            }
        }
    }
    let optind = walk.optind;
    let mut result = PhpArray::new(req)?;
    for (name, values) in found {
        let key = if !(name.len() > 1 && name[0] == b'0')
            && let Some(n) = numeric_string(&name).and_then(|v| v.as_int())
        {
            Key::Int(n)
        } else {
            Key::Str(PhpString::new(req, &name)?)
        };
        let mut items = Vec::new();
        for value in values {
            items.push(match value {
                Some(bytes) => string_value(req, &bytes)?,
                None => Value::bool(false),
            });
        }
        let value = if items.len() == 1 {
            items.remove(0)
        } else {
            let mut list = PhpArray::new(req)?;
            for item in items {
                list.append(req, item)?;
            }
            Value::array(list)
        };
        result.set(req, key, value)?;
    }
    if rest_index {
        assign_ref(
            req,
            3,
            Value::int(i64::try_from(optind).unwrap_or(i64::MAX)),
        )?;
    }
    Ok(Value::array(result))
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
    let Some(scanner) = crate::ini::scanner(mode) else {
        warning(req, "Invalid scanner mode");
        return Ok(Value::bool(false));
    };
    match php_ext::syntax::parse_ini(req, bytes, scanner)? {
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

/// `getservbyname(string $service, string $protocol): int|false`,
/// `PHP_FUNCTION(getservbyname)` (`Z/ext/standard/basic_functions.c:2030`):
/// the `port` `/etc/services` gives the `service` under the protocol,
/// `getservbyname(3)`'s, or `false`.
///
/// ```php
/// var_dump(getservbyname('http', 'tcp'), getservbyname('domain', 'udp'), getservbyname('nope', 'tcp'), getservbyname('http', ''));
/// ```
/// ```expect
/// int(80)
/// int(53)
/// bool(false)
/// bool(false)
/// ```
fn getservbyname(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let name = path_argument(req, 1)?;
    let proto = path_argument(req, 2)?;
    Ok(crate::ffi::service_port(name.as_bytes(), proto.as_bytes())
        .map_or_else(|| Value::bool(false), |port| Value::int(i64::from(port))))
}

/// `getservbyport(int $port, string $protocol): string|false`,
/// `PHP_FUNCTION(getservbyport)` (`Z/ext/standard/basic_functions.c:2069`):
/// the `service` name `getservbyport(3)` gives the `port`, cut to sixteen
/// bits as php's `(unsigned short)` cast cuts it, or `false`.
///
/// ```php
/// var_dump(getservbyport(80, 'tcp'), getservbyport(65536 + 22, 'tcp'), getservbyport(80, 'nope'));
/// ```
/// ```expect
/// string(4) "http"
/// string(3) "ssh"
/// bool(false)
/// ```
fn getservbyport(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let port = long_argument(req, 1)?;
    let proto = path_argument(req, 2)?;
    let port = u16::from_le_bytes([port.to_le_bytes()[0], port.to_le_bytes()[1]]);
    match crate::ffi::service_name(port, proto.as_bytes()) {
        Some(name) => string_value(req, &name),
        None => Ok(Value::bool(false)),
    }
}

/// `getprotobyname(string $protocol): int|false`,
/// `PHP_FUNCTION(getprotobyname)` (`Z/ext/standard/basic_functions.c:2098`):
/// `/etc/protocols`' number for the name, or `false`.
///
/// ```php
/// var_dump(getprotobyname('tcp'), getprotobyname('icmp'), getprotobyname('nope'));
/// ```
/// ```expect
/// int(6)
/// int(1)
/// bool(false)
/// ```
fn getprotobyname(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let name = path_argument(req, 1)?;
    Ok(crate::ffi::protocol_number(name.as_bytes())
        .map_or_else(|| Value::bool(false), |n| Value::int(i64::from(n))))
}

/// `getprotobynumber(int $protocol): string|false`,
/// `PHP_FUNCTION(getprotobynumber)` (`Z/ext/standard/basic_functions.c:2119`):
/// the name for the number, cut to an `int`, or `false`.
///
/// ```php
/// var_dump(getprotobynumber(17), getprotobynumber(6), getprotobynumber(99999));
/// ```
/// ```expect
/// string(3) "udp"
/// string(3) "tcp"
/// bool(false)
/// ```
fn getprotobynumber(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let number = long_argument(req, 1)?;
    // `(int)proto`: the low 32 bits.
    #[allow(clippy::cast_possible_truncation)]
    let number = number as i32;
    match crate::ffi::protocol_name(number) {
        Some(name) => string_value(req, &name),
        None => Ok(Value::bool(false)),
    }
}

/// `flush(): void`, php's `PHP_FUNCTION(flush)`: `sapi_flush`, the host
/// told it may push what it has, which the handler stack does not see.
///
/// ```php
/// ob_start(); echo "kept"; var_dump(flush()); $kept = ob_get_clean(); echo strlen($kept), "\n";
/// ```
/// ```expect
/// 9
/// ```
fn flush(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    req.flush();
    Ok(Value::null())
}

/// `usleep(int $microseconds): void`: the thread asleep that long; a
/// count below zero is php's `ValueError`.
///
/// ```php
/// usleep(10); var_dump(usleep(0));
/// try { usleep(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// NULL
/// usleep(): Argument #1 ($microseconds) must be greater than or equal to 0
/// ```
fn usleep(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let micros = long_argument(req, 1)?;
    let Ok(micros) = u64::try_from(micros) else {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    };
    // `usleep((unsigned int)num)`: the count as C's cast leaves it.
    let micros = micros & u64::from(u32::MAX);
    std::thread::sleep(std::time::Duration::from_micros(micros));
    Ok(Value::null())
}

/// `time_sleep_until(float $timestamp): bool`, php's
/// `PHP_FUNCTION(time_sleep_until)`: asleep until the wall clock reads
/// the timestamp, in whole nanoseconds; a time already past is the
/// warning and `false`, and one outside `0..=u64::MAX / 10^9` seconds is
/// php's `ValueError`, read before the clock is.
///
/// ```php
/// var_dump(time_sleep_until(microtime(true) + 0.01), time_sleep_until(1));
/// try { time_sleep_until(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: time_sleep_until(): Argument #1 ($timestamp) must be greater than or equal to the current time in %s on line 2
/// bool(true)
/// bool(false)
/// time_sleep_until(): Argument #1 ($timestamp) must be between 0 and 18446744073
/// ```
fn time_sleep_until(req: &mut Request) -> Result<Value, Thrown> {
    const NS_PER_SEC: u64 = 1_000_000_000;
    argument_count_between(req, 1, 1)?;
    let target = double_argument(req, 1)?;
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return Ok(Value::bool(false));
    };
    let top = u64::MAX / NS_PER_SEC;
    #[allow(clippy::cast_precision_loss)] // php's `(double)(UINT64_MAX / ns_per_sec)`
    let top_secs = top as f64;
    if !(target >= 0.0 && target <= top_secs) {
        return Err(arg_value_error(req, 1, &format!("be between 0 and {top}")));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // `(uint64_t)` of a checked double
    let target_ns = (target * 1e9) as u64;
    // `gettimeofday`: the clock to the microsecond.
    let current_ns = now.as_secs() * NS_PER_SEC + u64::from(now.subsec_micros()) * 1000;
    if target_ns < current_ns {
        docref(
            req,
            Level::Warning,
            "Argument #1 ($timestamp) must be greater than or equal to the current time",
        );
        return Ok(Value::bool(false));
    }
    std::thread::sleep(std::time::Duration::from_nanos(target_ns - current_ns));
    Ok(Value::bool(true))
}

/// `get_current_user(): string`, `php_get_current_user`
/// (`Z/main/main.c:1450`): the login name of the owner of the file the
/// request serves, which `sapi_get_stat` stats; the empty string where
/// there is no such file, `php -r` and standard input, or no name for
/// its owner. The name is the host's, so the witness pins its shape.
///
/// ```php
/// $u = get_current_user(); var_dump(is_string($u), $u !== "", $u === get_current_user());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
fn get_current_user(req: &mut Request) -> Result<Value, Thrown> {
    use std::os::unix::fs::MetadataExt;
    argument_count_between(req, 0, 0)?;
    let owner = req
        .script_name()
        .map(<[u8]>::to_vec)
        .and_then(|script| req.realpath(&script))
        .and_then(|path| std::fs::metadata(path.0).ok())
        .and_then(|meta| crate::ffi::user_name(meta.uid()));
    string_value(req, &owner.unwrap_or_default())
}

/// `get_cfg_var(string $option): string|array|false`: the row of
/// `configuration_hash` under the name, what the host's ini text set,
/// with the array `name[]=` lines build ([`Request::cfg_entry`]); `false`
/// where the text set nothing, a default included.
///
/// ```php
/// var_dump(get_cfg_var('date.timezone'), get_cfg_var('display_errors'), get_cfg_var('precision'), get_cfg_var('nope'));
/// ```
/// ```expect
/// string(3) "UTC"
/// string(1) "1"
/// string(2) "14"
/// bool(false)
/// ```
fn get_cfg_var(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let option = string_argument(req, 1)?;
    Ok(req
        .cfg_entry(option.as_bytes())?
        .unwrap_or_else(|| Value::bool(false)))
}

/// Where `error_log` sends a message, its `$message_type` decoded once
/// (`_php_error_log_ex`, `Z/ext/standard/basic_functions.c:1378`).
enum LogTo<'a> {
    /// `0` and any number not below: `php_log_err_with_severity`, the
    /// host's log.
    Log,
    /// `1`: `php_mail` to the destination, with the headers.
    Mail(&'a [u8], Option<&'a [u8]>),
    /// `2`: the removed TCP/IP option.
    Tcp,
    /// `3`: appended to the file the destination names.
    File(&'a [u8]),
    /// `4`: `sapi_module.log_message`, the host's error stream.
    Sapi,
}

/// `error_log(string $message, int $message_type = 0, ?string
/// $destination = null, ?string $additional_headers = null): bool`:
/// the message to the host's log, to a mail, appended to a file, or to
/// the host's error stream, and `true` where it went.
///
/// ```php
/// var_dump(error_log("to the log", 4), error_log("x", 3, "/nonexistent/dir/f"));
/// $f = tempnam(sys_get_temp_dir(), "el"); var_dump(error_log("one\n", 3, $f), error_log("two", 3, $f), file_get_contents($f)); unlink($f);
/// try { error_log("x", 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: error_log(/nonexistent/dir/f): Failed to open stream: No such file or directory in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// string(7) "one
/// two"
/// TCP/IP option is not available for error logging
/// ```
/// ```stderr
/// to the log
/// ```
fn error_log(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 4)?;
    let message = string_argument(req, 1)?;
    let kind = optional_long(req, 2, 0)?;
    let destination = match given(req, 3) {
        Some(value) if !matches!(value.deref().as_ref(), ValueRef::Null) => {
            Some(path_argument(req, 3)?)
        }
        _ => None,
    };
    let headers = nullable_string(req, 4)?;
    let destination_bytes = destination.as_ref().map(PhpString::as_bytes);
    let to = match i32::try_from(kind).unwrap_or(0) {
        1 => LogTo::Mail(
            destination_bytes.unwrap_or_default(),
            headers.as_ref().map(PhpString::as_bytes),
        ),
        2 => LogTo::Tcp,
        3 => LogTo::File(destination_bytes.unwrap_or_default()),
        4 => LogTo::Sapi,
        _ => LogTo::Log,
    };
    let message = message.as_bytes();
    let sent = match to {
        LogTo::Log => {
            req.error_log(message);
            true
        }
        LogTo::Mail(to, headers) => {
            crate::mail::php_mail(req, to, b"PHP error_log message", message, headers, None)?
        }
        LogTo::Tcp => {
            return Err(throw(
                req,
                "ValueError",
                "TCP/IP option is not available for error logging",
            ));
        }
        LogTo::File(path) => match req.open_stream(path, b"a", false)? {
            Some(stream) => {
                let written = stream.write(req, message);
                stream.close(req);
                written == Some(message.len())
            }
            None => false,
        },
        LogTo::Sapi => {
            let mut line = message.to_vec();
            line.push(b'\n');
            req.write_stderr(&line);
            true
        }
    };
    Ok(Value::bool(sent))
}

/// `ini_get_all(?string $extension = null, bool $details = true):
/// array|false`, php's `PHP_FUNCTION(ini_get_all)`: `EG(ini_directives)`
/// sorted by `zend_ini_sort_entries`, a case-blind comparison of the
/// names, each entry as its current value or, with `$details`, as its
/// `global_value`, `local_value` and `access`; with an extension named
/// as `module_registry` keys it, lower-case, its own entries alone,
/// `core` being the one name every entry is walked for. A name no
/// extension of the bundle has is the warning and `false`.
///
/// ```php
/// var_dump(ini_get_all("reflection"), ini_get_all("pcre", false));
/// var_dump(ini_get_all(""), ini_get_all("PCRE"), count(ini_get_all("core")) === count(ini_get_all()));
/// ```
/// ```expect
/// array(0) {
/// }
/// array(3) {
///   ["pcre.backtrack_limit"]=>
///   string(7) "1000000"
///   ["pcre.jit"]=>
///   string(1) "1"
///   ["pcre.recursion_limit"]=>
///   string(6) "100000"
/// }
///
/// Warning: ini_get_all(): Extension "" cannot be found in %s on line 3
///
/// Warning: ini_get_all(): Extension "PCRE" cannot be found in %s on line 3
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
fn ini_get_all(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 2)?;
    let extension = nullable_string(req, 1)?;
    let details = optional_bool(req, 2, true)?;
    let mut entries = req.ini_entries();
    entries.sort_by(|a, b| {
        let (a, b) = (a.decl.name.0.as_ref(), b.decl.name.0.as_ref());
        a.to_ascii_lowercase()
            .cmp(&b.to_ascii_lowercase())
            .then(a.len().cmp(&b.len()))
    });
    if let Some(name) = extension {
        let name = name.as_bytes();
        let registered = name.iter().all(|c| !c.is_ascii_uppercase());
        let module = registered.then(|| req.extension_named(name)).flatten();
        if name == b"core" {
        } else if let Some(module) = module {
            entries.retain(|entry| entry.extension == Some(module));
        } else {
            let text = format!(
                "Extension \"{}\" cannot be found",
                String::from_utf8_lossy(name)
            );
            docref(req, Level::Warning, &text);
            return Ok(Value::bool(false));
        }
    }
    let mut all = PhpArray::new(req)?;
    for entry in entries {
        let key = Key::from_string(PhpString::new(req, &entry.decl.name.0)?);
        let value = if details {
            let access = (i64::from(entry.decl.user) * INI_USER)
                | (i64::from(entry.decl.per_dir) * INI_PERDIR)
                | (i64::from(entry.decl.system) * INI_SYSTEM);
            let global = entry.original.as_deref().unwrap_or(&entry.value).to_vec();
            let mut option = PhpArray::new(req)?;
            for (name, value) in [
                (&b"global_value"[..], string_value(req, &global)?),
                (b"local_value", string_value(req, &entry.value)?),
                (b"access", Value::int(access)),
            ] {
                let name = Key::Str(PhpString::new(req, name)?);
                option.set(req, name, value)?;
            }
            Value::array(option)
        } else {
            string_value(req, &entry.value)?
        };
        all.set(req, key, value)?;
    }
    Ok(Value::array(all))
}

/// `ini_restore(string $option): void`, `zend_restore_ini_entry` at
/// runtime: an entry the request changed set back to the value it
/// started with, through the entry's own `on_modify`; an entry it did
/// not change, or one `PHP_INI_USER` may not touch, is left as it is.
///
/// ```php
/// var_dump(set_include_path("var"), ini_restore("include_path"), get_include_path() === set_include_path("x"));
/// ini_set("precision", "5"); ini_restore("precision"); echo 1/3, "\n";
/// var_dump(ini_restore("nope"), ini_restore("enable_dl"));
/// ```
/// ```expect
/// string(47) ".:/opt/homebrew/Cellar/php/8.5.9/share/php/pear"
/// NULL
/// bool(true)
/// 0.33333333333333
/// NULL
/// NULL
/// ```
fn ini_restore(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let option = string_argument(req, 1)?;
    let name = option.as_bytes();
    let changed = req
        .ini_entries()
        .into_iter()
        .find(|entry| entry.decl.name.0.as_ref() == name && entry.decl.user)
        .and_then(|entry| entry.original);
    if let Some(original) = changed {
        req.ini_set(name, &original)?;
    }
    Ok(Value::null())
}

/// `ini_parse_quantity(string $shorthand): int`: the quantity as an ini
/// entry reads one, `zend_ini_parse_quantity`
/// ([`Request::ini_parse_quantity`]), with the sentence it says of a
/// shape it interprets raised as a warning.
///
/// ```php
/// var_dump(ini_parse_quantity('128M'), ini_parse_quantity('0x0b'), ini_parse_quantity('-0xB'), ini_parse_quantity('0b+0'));
/// ```
/// ```expect
/// Warning: Invalid quantity "0b+0": no digits after base prefix, interpreting as "0" for backwards compatibility in %s on line 2
/// int(134217728)
/// int(11)
/// int(-11)
/// int(0)
/// ```
fn ini_parse_quantity(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let shorthand = string_argument(req, 1)?;
    let (value, said) = Request::ini_parse_quantity(shorthand.as_bytes());
    if let Some(said) = said {
        warning(req, &said);
    }
    Ok(Value::int(value))
}

/// `connection_aborted(): int`: `PG(connection_status)`'s aborted bit.
/// No host of this engine reports a client gone yet, so the status is
/// `CONNECTION_NORMAL` throughout, as it is under php's CLI.
///
/// ```php
/// var_dump(connection_aborted(), connection_status(), CONNECTION_NORMAL, CONNECTION_ABORTED, CONNECTION_TIMEOUT, INI_USER, INI_PERDIR, INI_SYSTEM, INI_ALL);
/// ```
/// ```expect
/// int(0)
/// int(0)
/// int(0)
/// int(1)
/// int(2)
/// int(1)
/// int(2)
/// int(4)
/// int(7)
/// ```
fn connection_aborted(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    Ok(Value::int(CONNECTION_NORMAL & CONNECTION_ABORTED))
}

/// `connection_status(): int`: `PG(connection_status)`, see
/// [`connection_aborted`].
fn connection_status(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    Ok(Value::int(CONNECTION_NORMAL))
}

/// `ignore_user_abort(?bool $enable = null): int`: `PG(ignore_user_abort)`
/// as it stood, and, for a boolean given, the `ignore_user_abort` entry
/// set to `1` or `0` through `zend_alter_ini_entry_chars`.
///
/// ```php
/// var_dump(ignore_user_abort(), ignore_user_abort(true), ignore_user_abort(null), ini_get("ignore_user_abort"), ignore_user_abort(false));
/// ```
/// ```expect
/// int(0)
/// int(0)
/// int(1)
/// string(1) "1"
/// int(1)
/// ```
fn ignore_user_abort(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 1)?;
    let enable = match given(req, 1) {
        Some(value) if !matches!(value.deref().as_ref(), ValueRef::Null) => {
            Some(bool_argument(req, 1)?)
        }
        _ => None,
    };
    let old = req
        .ini_get(b"ignore_user_abort")
        .is_some_and(Request::ini_parse_bool);
    if let Some(enable) = enable {
        req.ini_set(b"ignore_user_abort", if enable { b"1" } else { b"0" })?;
    }
    Ok(Value::int(i64::from(old)))
}

/// `register_tick_function(callable $callback, mixed ...$args): bool`:
/// the callback and its arguments kept for `ZEND_TICKS` to serve.
///
/// ```php
/// function tick($x) { echo "tick $x\n"; }
/// var_dump(register_tick_function("tick", "a"), unregister_tick_function("tick"), unregister_tick_function("tick"));
/// try { register_tick_function("nope"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// NULL
/// NULL
/// register_tick_function(): Argument #1 ($callback) must be a valid callback, function "nope" not found or invalid function name
/// ```
fn register_tick_function(req: &mut Request) -> Result<Value, Thrown> {
    if req.argument_count() < 1 {
        return Err(php_ext::raising::too_few_arguments(req, 1, 0));
    }
    let callback = crate::callable_argument(req, 1)?;
    let arguments = req.variadic_arguments(2)?;
    Ok(Value::bool(req.register_tick_function(callback, arguments)))
}

/// `unregister_tick_function(callable $callback): void`: the first
/// entry kept for the same callee, scope and object dropped.
fn unregister_tick_function(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let callback = crate::callable_argument(req, 1)?;
    req.unregister_tick_function(&callback);
    Ok(Value::null())
}

/// `is_uploaded_file(string $filename): bool`: whether the request's
/// upload table holds the path.
///
/// ```php
/// var_dump(is_uploaded_file(__FILE__), move_uploaded_file(__FILE__, "/tmp/x"));
/// try { is_uploaded_file("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// is_uploaded_file(): Argument #1 ($filename) must not contain any null bytes
/// ```
fn is_uploaded_file(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let path = path_argument(req, 1)?;
    Ok(Value::bool(req.is_uploaded_file(path.as_bytes())))
}

/// `move_uploaded_file(string $from, string $to): bool`: an upload the
/// request holds renamed to its new path, or copied there and unlinked
/// where the rename fails, and taken out of the table; `false` for a
/// path that was no upload, and php's warning for one that could not
/// be moved.
fn move_uploaded_file(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let from = path_argument(req, 1)?;
    let to = path_argument(req, 2)?;
    if !req.is_uploaded_file(from.as_bytes()) {
        return Ok(Value::bool(false));
    }
    let target = req.absolutize(to.as_bytes());
    let Some(source) = req.take_uploaded_file(from.as_bytes()) else {
        return Ok(Value::bool(false));
    };
    let moved = std::fs::rename(&source.0, &target.0).is_ok()
        || (std::fs::copy(&source.0, &target.0).is_ok() && std::fs::remove_file(&source.0).is_ok());
    if !moved {
        let text = format!(
            "Unable to move \"{}\" to \"{}\"",
            String::from_utf8_lossy(from.as_bytes()),
            String::from_utf8_lossy(to.as_bytes())
        );
        docref(req, Level::Warning, &text);
    }
    Ok(Value::bool(moved))
}

/// `sys_getloadavg(): array|false`: the host's one-, five- and
/// fifteen-minute load averages, `getloadavg(3)`; `false` where it has
/// none. The numbers are the host's, so the witness pins the shape.
///
/// ```php
/// $l = sys_getloadavg(); var_dump(count($l), is_float($l[0]) && is_float($l[1]) && is_float($l[2]), array_keys($l));
/// ```
/// ```expect
/// int(3)
/// bool(true)
/// array(3) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(1)
///   [2]=>
///   int(2)
/// }
/// ```
fn sys_getloadavg(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    let Some(load) = crate::ffi::loadavg() else {
        return Ok(Value::bool(false));
    };
    let mut array = PhpArray::new(req)?;
    for average in load {
        array.append(req, Value::float(average))?;
    }
    Ok(Value::array(array))
}

/// `ftok(string $filename, string $project_id): int`, `Z/ext/standard/ftok.c`:
/// the System V IPC key the host makes of the path and the one
/// character; `-1` after php's warning where the host refuses.
///
/// ```php
/// var_dump(ftok(__FILE__, "a") > 0, ftok("/nonexistent/f", "a"));
/// try { ftok("", "a"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { ftok(__FILE__, "ab"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: ftok(): ftok() failed - No such file or directory in %s on line 2
/// bool(true)
/// int(-1)
/// ftok(): Argument #1 ($filename) must not be empty
/// ftok(): Argument #2 ($project_id) must be a single character
/// ```
fn ftok(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let path = path_argument(req, 1)?;
    let project = string_argument(req, 2)?;
    if path.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    let &[project] = project.as_bytes() else {
        return Err(arg_value_error(req, 2, "be a single character"));
    };
    let full = req.absolutize(path.as_bytes());
    match crate::ffi::ftok(full.0.as_os_str().as_bytes(), project) {
        Ok(key) => Ok(Value::int(key)),
        Err(errno) => {
            let error = std::io::Error::from_raw_os_error(errno);
            let text = format!("ftok() failed - {}", crate::filestat::strerror(&error));
            docref(req, Level::Warning, &text);
            Ok(Value::int(-1))
        }
    }
}

/// `inet_ntop(string $ip): string|false`: four bytes as a dotted quad
/// and sixteen as the host's IPv6 text; any other length, `false`.
///
/// ```php
/// var_dump(inet_ntop("\x7f\0\0\1"), inet_ntop(str_repeat("\0", 15) . "\1"), inet_ntop("\0\0\0\0\0\0\0\0\0\0\xff\xff\x01\x02\x03\x04"), inet_ntop("abc"), inet_ntop(""));
/// ```
/// ```expect
/// string(9) "127.0.0.1"
/// string(3) "::1"
/// string(14) "::ffff:1.2.3.4"
/// bool(false)
/// bool(false)
/// ```
fn inet_ntop(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let address = string_argument(req, 1)?;
    match crate::ffi::inet_text(address.as_bytes()) {
        Some(text) => string_value(req, &text),
        None => Ok(Value::bool(false)),
    }
}

/// `inet_pton(string $ip): string|false`: text with a `:` read as IPv6
/// into sixteen bytes, text with a `.` as IPv4 into four, by the host's
/// `inet_pton`; anything else, and what the host refuses, `false`.
///
/// ```php
/// var_dump(inet_pton("127.0.0.1") === "\x7f\0\0\1", bin2hex(inet_pton("::1")), inet_pton("1.2.3"), inet_pton("abra"), inet_pton(""));
/// try { inet_pton("\0"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(32) "00000000000000000000000000000001"
/// bool(false)
/// bool(false)
/// bool(false)
/// inet_pton(): Argument #1 ($ip) must not contain any null bytes
/// ```
fn inet_pton(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let address = path_argument(req, 1)?;
    let text = address.as_bytes();
    let six = text.contains(&b':');
    if !six && !text.contains(&b'.') {
        return Ok(Value::bool(false));
    }
    match crate::ffi::inet_bytes(text, six) {
        Some(bytes) => string_value(req, &bytes),
        None => Ok(Value::bool(false)),
    }
}

/// `header_register_callback(callable $callback): bool`,
/// `Z/main/SAPI.c`: the callback `sapi_send_headers` serves once, just
/// before the header block goes out, replacing one registered before.
///
/// ```php
/// header_register_callback(function () { echo "first\n"; });
/// var_dump(header_register_callback(function () { echo "headers\n"; }));
/// echo "body\n";
/// ```
/// ```expect
/// headers
/// bool(true)
/// body
/// ```
fn header_register_callback(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let callback = crate::callable_argument(req, 1)?;
    req.set_header_callback(callback);
    Ok(Value::bool(true))
}

/// The source of a file as `open_file_for_scanning` reads it through
/// `zend_stream_open`, the include path searched and the stream's
/// warning raised where the file could not be opened: `None` then.
fn scanned_file(req: &mut Request, filename: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(stream) = req.open_stream(filename, b"rb", true)? else {
        return Ok(None);
    };
    let source = stream.copy_to_mem(req, None)?;
    stream.close(req);
    Ok(Some(source.as_bytes().to_vec()))
}

/// `highlight_file(string $filename, bool $return = false): string|bool`,
/// php's `PHP_FUNCTION(highlight_file)`, and its alias `show_source`:
/// the file as `zend_highlight` writes it (`php_ext::syntax::highlight`),
/// to the body or, with `$return`, as the answer; a file that cannot
/// be opened is the stream's warning, php's `Failed opening '%s' for
/// highlighting` and `false`.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'h'); file_put_contents($f, "<?php echo 'a'; ?>\n");
/// var_dump(highlight_file($f, true)); var_dump(show_source($f));
/// var_dump(highlight_file("nope.php"));
/// try { highlight_file("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// unlink($f);
/// ```
/// ```expect
/// string(252) "<pre><code style="color: #000000"><span style="color: #0000BB">&lt;?php </span><span style="color: #007700">echo </span><span style="color: #DD0000">'a'</span><span style="color: #007700">; </span><span style="color: #0000BB">?&gt;
/// </span></code></pre>"
/// <pre><code style="color: #000000"><span style="color: #0000BB">&lt;?php </span><span style="color: #007700">echo </span><span style="color: #DD0000">'a'</span><span style="color: #007700">; </span><span style="color: #0000BB">?&gt;
/// </span></code></pre>bool(true)
///
/// Warning: highlight_file(nope.php): Failed to open stream: No such file or directory in %s on line 4
///
/// Warning: highlight_file(): Failed opening 'nope.php' for highlighting in %s on line 4
/// bool(false)
/// highlight_file(): Argument #1 ($filename) must not contain any null bytes
/// ```
fn highlight_file(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let filename = path_argument(req, 1)?;
    let returned = optional_bool(req, 2, false)?;
    let full = req.absolutize(filename.as_bytes());
    req.fs_guard(&full, php_ext::request::PathIntent::Read)?;
    let Some(source) = scanned_file(req, filename.as_bytes())? else {
        let text = format!(
            "Failed opening '{}' for highlighting",
            String::from_utf8_lossy(filename.as_bytes())
        );
        docref(req, Level::Warning, &text);
        return Ok(Value::bool(false));
    };
    let html = php_ext::syntax::highlight(req, &source);
    if returned {
        return string_value(req, &html);
    }
    req.write(&html);
    Ok(Value::bool(true))
}

/// `php_strip_whitespace(string $filename): string`, php's
/// `PHP_FUNCTION(php_strip_whitespace)`: the file as `zend_strip` writes
/// it (`php_ext::syntax::strip`), comments dropped and whitespace one
/// space; a file that cannot be opened is the stream's warning and the
/// empty string.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'w'); file_put_contents($f, "<?php\n/* c */ \$a  =  1; // d\n\$b = <<<E\n x\n E;\n\n\techo \$a;\n");
/// var_dump(php_strip_whitespace($f), php_strip_whitespace("nope.php")); unlink($f);
/// ```
/// ```expect
/// Warning: php_strip_whitespace(nope.php): Failed to open stream: No such file or directory in %s on line 3
/// string(41) "<?php
///  $a = 1; $b = <<<E
///  x
///  E;
/// echo $a; "
/// string(0) ""
/// ```
fn php_strip_whitespace(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let filename = path_argument(req, 1)?;
    let Some(source) = scanned_file(req, filename.as_bytes())? else {
        return string_value(req, b"");
    };
    let stripped = php_ext::syntax::strip(req, &source);
    string_value(req, &stripped)
}

/// `highlight_string(string $string, bool $return = false): string|true`,
/// php's `PHP_FUNCTION(highlight_string)`: the string as `zend_highlight`
/// writes it, scanned from `INITIAL` as a file is, to the body or,
/// with `$return`, as the answer.
///
/// ```php
/// var_dump(highlight_string("<?php 1;", true), highlight_string("x"));
/// ```
/// ```expect
/// <pre><code style="color: #000000">x</code></pre>string(130) "<pre><code style="color: #000000"><span style="color: #0000BB">&lt;?php 1</span><span style="color: #007700">;</span></code></pre>"
/// bool(true)
/// ```
fn highlight_string(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let string = string_argument(req, 1)?;
    let returned = optional_bool(req, 2, false)?;
    let html = php_ext::syntax::highlight(req, string.as_bytes());
    if returned {
        return string_value(req, &html);
    }
    req.write(&html);
    Ok(Value::bool(true))
}
