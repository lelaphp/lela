//! `php_formatted_print` (`Z/ext/standard/formatted_print.c:423`), the
//! one formatter behind `sprintf`, `printf`, `vsprintf`, `vprintf`,
//! `fprintf` and `vfprintf` (`basic_functions.stub.php:2960-2973`), and its appenders: php's
//! conversion table, its argnum, padding, alignment, width and
//! precision forms, read left to right as php reads them, one
//! specifier at a time. The compiler shapes none of these calls yet
//! (`zend_compile_func_sprintf` is `Special::Opcode` with no kernel), so
//! every call reaches the native, where php's own reaches it only for a
//! format the compiler could not read.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use crate::string::Decimal;
use php_ext::abi::{Raw, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::{Exponent, float_format, to_double, to_long, to_string};
use php_ext::raising::{Level, Thrown, docref, fatal, throw_error};
use php_ext::request::{Request, Stream};
use php_ext::values::{Emalloc, FreshString, Owner, PhpArray, PhpString, Value};

/// `sprintf(string $format, mixed ...$values): string`, php's
/// `PHP_FUNCTION(sprintf)`: the format and every argument after it
/// through [`formatted`], with one additional parameter, the format, in
/// the count its `ArgumentCountError` names.
///
/// The conversions, each over `zval_get_*` of its argument: `%s` is
/// `zval_get_string`, so a float prints at `precision` and `null` and
/// `false` print nothing; `%d` and `%u` are `zval_get_long`, the second
/// read back as unsigned; `%c` is the long's low byte; `%b`, `%o`, `%x`
/// and `%X` are the long's bits in groups; `%e`, `%E`, `%f` and `%F`
/// are `php_conv_fp` at six places unless a precision says otherwise,
/// and `%g`, `%G`, `%h` and `%H` are `zend_gcvt` at six significant
/// digits. `%%` is one `%`.
///
/// ```php
/// var_dump(sprintf("%s|%s|%s|%s|%s", "str", 1.5, 1e19, null, false), sprintf("%d|%d|%d|%d|%u|%u", 42, -42, "12abc", 1.9, -1, 3), sprintf("%c%c|%b|%o|%x|%X", 65, "66", 1023, 8, 255, 255), sprintf("%e|%E|%f|%F|%g|%G|%h|%H", 1234.5678, 1234.5678, 1234.5678, 1234.5678, 1234.5678, 1e25, 0.00001, 1.5e-7), sprintf("100%%|%%s|%s", "x"));
/// ```
/// ```expect
/// string(17) "str|1.5|1.0E+19||"
/// string(34) "42|-42|12|1|18446744073709551615|3"
/// string(22) "AB|1111111111|10|ff|FF"
/// string(77) "1.234568e+3|1.234568E+3|1234.567800|1234.567800|1234.57|1.0E+25|1.0e-5|1.5E-7"
/// string(9) "100%|%s|x"
/// ```
///
/// `%f`, `%g` and `%G` write the point of the request's `LC_NUMERIC`,
/// and `%e`, `%E`, `%F`, `%h` and `%H` a `.` under any locale, as does
/// a float's cast to string:
///
/// ```php
/// setlocale(LC_NUMERIC, "is_IS", "is_IS.UTF-8");
/// var_dump(sprintf("%.3f|%01.2f|%f'", 100.426, 123.1, 100.426), sprintf("%.3e|%F|%g|%G|%h|%H", 123.1, 1.5, 1.5, 1.5e25, 1.5, 1.5), sprintf("%s", 1.5), 1.5 . "");
/// setlocale(LC_ALL, "de_DE"); printf("%.2f %.2F\n", 3.14, 3.14);
/// setlocale(LC_ALL, "C"); printf("%.2f\n", 3.14);
/// ```
/// ```expect locale=is_IS locale=de_DE
/// string(26) "100,426|123,10|100,426000'"
/// string(37) "1.231e+2|1.500000|1,5|1,5E+25|1.5|1.5"
/// string(3) "1.5"
/// string(3) "1.5"
/// 3,14 3.14
/// 3.14
/// ```
///
/// Width pads to the left with spaces, or with `0` or the byte after
/// `'`; `-` pads to the right instead, where a `0` becomes a space for
/// an integer; `+` writes a sign before a number that has none. A sign
/// stands before zero padding and inside space padding.
///
/// ```php
/// var_dump(sprintf("%5d|%-5d|%05d|%-05d|%+d|%+5d|%+05d|%-+5d|%05d", 42, 42, 42, 42, 42, 42, 42, 42, -42), sprintf("%'*10s|%'.-10d|%5s|%-5s|%05s|%5c|", "pad", 42, "ab", "ab", "ab", 65), sprintf("%08.3f|%+08.3f|%-08.3f|%+.2f|%+s", -1.5, 1.5, 1.5, 2.0, "x"));
/// ```
/// ```expect
/// string(51) "   42|42   |00042|42   |+42|  +42|+0042|+42  |-0042"
/// string(42) "*******pad|42........|   ab|ab   |000ab|A|"
/// string(34) "-001.500|+001.500|1.500000|+2.00|x"
/// ```
///
/// Precision is the places after the point of `%e` and `%f`, the
/// significant digits of `%g`, capped at 53 behind php's notice; the
/// bytes kept of `%s`; an integer's `%d` ignores it, and `%b`, `%o` and
/// `%x` print nothing under one, since their appender caps them at
/// zero bytes. A `.` with no number is precision zero. The
/// rounding is `zend_dtoa`'s over the double's exact value, so `1.005`
/// at two places is `1.00` and `2.5` at none is `2`.
///
/// ```php
/// var_dump(sprintf("%.2f|%.0f|%.0f|%.0f|%.1f|%.2f|%.3f|%.f", 1.005, 0.5, 1.5, 2.5, 0.05, 0.001, 2.0005, 1.5), sprintf("%.3e|%.0e|%.e|%.2e|%.1e|%e", 1234.5678, 1234.5678, 1.5, 9.999, 9.96, 0.0), sprintf("%.3g|%.8g|%.17g|%.1g|%.0g|%.2h|%.2H", 1234.5, 12345.6789, 0.1, 0.15, 12345, 0.001234, 1e-7), sprintf("%.3s|%.0s|%.s|%.10s|%5.2s|%-5.2s|", "abcdef", "abcdef", "abcdef", "abcdef", "abc", "abc"), sprintf("%.2x|%.1x|%10.2X|%5.3d|%.3d", 255, 255, 255, 42, 42), sprintf("%.2f|%.2f", 1e20, 123456789012345678.0));
/// ```
/// ```expect
/// string(27) "1.00|0|2|2|0.1|0.00|2.001|2"
/// string(45) "1.235e+3|1e+3|2e+0|1.00e+1|1.0e+1|0.000000e+0"
/// string(62) "1.23e+3|12345.679|0.10000000000000001|0.1|1.0e+4|0.0012|1.0E-7"
/// string(31) "abc||abcdef|abcdef|   ab|ab   |"
/// string(21) "||          |   42|42"
/// string(46) "100000000000000000000.00|123456789012345680.00"
/// ```
///
/// ```php
/// var_dump(sprintf("%.60f", 1.1));
/// ```
/// ```expect
/// Notice: sprintf(): Requested precision of 60 digits was truncated to PHP maximum of 53 digits in %s on line 2
/// string(55) "1.10000000000000008881784197001252323389053344726562500"
/// ```
///
/// `%g`'s digits past seventeen are the double's decimal expansion and
/// not the shortest text that reads back, since `zend_gcvt` asks
/// `zend_dtoa` for as many digits as the precision names and it caps
/// at none of its own; the cap at 53 is php's, with the notice
/// above it. So `%.20g` of `0.1` is `0.10000000000000000555` where
/// `%.17g` is `0.10000000000000001`, and a precision wide enough to
/// hold the point leaves the E-style behind.
///
/// ```php
/// var_dump(sprintf("%.20g", 0.1), sprintf("%.18g", 0.1), sprintf("%.20G", 0.1 + 0.2), sprintf("%.20g", 5e-324), sprintf("%.30g", 1e25), sprintf("%.53g", 0.1), sprintf("%.20h", 1/3), sprintf("%.60g", 0.1));
/// ```
/// ```expect
/// Notice: sprintf(): Requested precision of 60 digits was truncated to PHP maximum of 53 digits in %s on line 2
/// string(22) "0.10000000000000000555"
/// string(20) "0.100000000000000006"
/// string(22) "0.30000000000000004441"
/// string(26) "4.9406564584124654418e-324"
/// string(26) "10000000000000000905969664"
/// string(55) "0.10000000000000000555111512312578270211815834045410156"
/// string(22) "0.33333333333333331483"
/// string(55) "0.10000000000000000555111512312578270211815834045410156"
/// ```
///
/// `n$` before the modifiers names the argument, and `*` reads the
/// width or the precision from the next argument or the one `*n$`
/// names; a specifier with no argnum takes the next argument after the
/// last one taken that way, so a named one does not advance it.
///
/// ```php
/// var_dump(sprintf("%2\$s %1\$s %1\$s", "a", "b"), sprintf("%2\$s %s %s", "a", "b"), sprintf("%*d|%-*d|%.*f|%*.*f", 5, 42, 5, 42, 2, 3.14159, 8, 2, 3.14159), sprintf("%1\$*2\$d|%2\$*1\$d", 7, 4), sprintf("%1\$'x5d|%1\$-5d|%1\$05d", 1));
/// ```
/// ```expect
/// string(5) "b a a"
/// string(5) "b a b"
/// string(25) "   42|42   |3.14|    3.14"
/// string(12) "   7|      4"
/// string(17) "xxxx1|1    |00001"
/// ```
///
/// The non-finite doubles print as `NaN`, `INF` and `-INF` at their own
/// width, so a width is ignored, and a `+` with zero padding writes its
/// sign over the first byte, as php's appender does.
///
/// ```php
/// var_dump(sprintf("%f|%F|%e|%10f|%+05f|%+e", NAN, INF, -INF, INF, NAN, -NAN), sprintf("%g|%f|%s|%.1f|%d", -0.0, -0.0, -0.0, -0.5, "abc"));
/// ```
/// ```expect
/// string(24) "NaN|INF|-INF|INF|+aN|NaN"
/// string(21) "-0|0.000000|-0|-0.5|0"
/// ```
///
/// A missing argument is counted through the whole format and raised at
/// its end as the `ArgumentCountError` naming the highest argnum the
/// format reached; a specifier whose argument is missing is skipped
/// before its conversion is read, so an unknown one is refused only
/// with its argument in hand.
///
/// ```php
/// function t($c) { try { var_dump(match (count($c)) { 1 => sprintf($c[0]), 2 => sprintf($c[0], $c[1]), 3 => sprintf($c[0], $c[1], $c[2]) }); } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; } }
/// foreach ([["%s %s %s"], ["%d %s", 1], ["%5\$s %2\$s", 1, 2], ["%*d"], ["%"], ["%", 1], ["%s %q", 1], ["%q"], ["%q", 1], ["%'"], ["%$"], ["%0\$s", 1], ["%99999999999\$d", 1], ["%2147483647\$d", 1], ["%2147483646\$d", 1], ["%99999999999d", 1], ["%.99999999999d", 1], ["%*d", "a", 1], ["%*d", -1, 1], ["%.*d", "1", 1], ["%.*d", -2, 1], ["%.*d", -1, 1], ["%.*g", -1, 0.1]] as $c) { t($c); }
/// ```
/// ```expect
/// ArgumentCountError: 4 arguments are required, 1 given
/// ArgumentCountError: 3 arguments are required, 2 given
/// ArgumentCountError: 6 arguments are required, 3 given
/// ArgumentCountError: 2 arguments are required, 1 given
/// ArgumentCountError: 2 arguments are required, 1 given
/// ValueError: Missing format specifier at end of string
/// ArgumentCountError: 3 arguments are required, 2 given
/// ArgumentCountError: 2 arguments are required, 1 given
/// ValueError: Unknown format specifier "q"
/// ValueError: Missing padding character
/// ValueError: Argument number specifier must be greater than zero and less than 2147483647
/// ValueError: Argument number specifier must be greater than zero and less than 2147483647
/// ValueError: Argument number specifier must be greater than zero and less than 2147483647
/// ValueError: Argument number specifier must be greater than zero and less than 2147483647
/// ArgumentCountError: 2147483647 arguments are required, 2 given
/// ValueError: Width must be between 0 and 2147483647
/// ValueError: Precision must be between 0 and 2147483647
/// ValueError: Width must be an integer
/// ValueError: Width must be between 0 and 2147483647
/// ValueError: Precision must be an integer
/// ValueError: Precision must be between -1 and 2147483647
/// ValueError: Precision -1 is only supported for %g, %G, %h and %H
/// string(3) "0.1"
/// ```
///
/// The format is `Z_PARAM_STRING` and the rest `Z_PARAM_VARIADIC`, so
/// the arity is at least one, and an argument the conversion cannot
/// read is the warning or the `Error` `zval_get_long` and
/// `zval_get_string` raise for it.
///
/// ```php
/// $o = new stdClass; var_dump(sprintf("%d|%s", $o, [1]));
/// try { sprintf("%s", $o); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { sprintf(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: Object of class stdClass could not be converted to int in %s on line 2
///
/// Warning: Array to string conversion in %s on line 2
/// string(7) "1|Array"
/// Object of class stdClass could not be converted to string
/// sprintf() expects at least 1 argument, 0 given
/// ```
///
/// ```php
/// sprintf([]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: sprintf(): Argument #1 ($format) must be of type string, array given in %s:2
/// Stack trace:
/// #0 %s(2): sprintf(Array)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// The string formatted into is `zend_string_alloc(240)`, laid down before the format
/// is read, so it stands across a throw part-way, beside `vsprintf`'s
/// copy of its array, and is handed out at the size it grew to, never
/// cut to its bytes.
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
/// pk(function () { try { sprintf('%d %d', 1); } catch (ArgumentCountError $e) {} });
/// pk(function () { try { vsprintf('%d %d', [1]); } catch (ValueError $e) {} });
/// $f = 'sprintf'; $m = memory_get_usage(); $x = $f("%s", "abc"); $y = $f("%s", str_repeat("a", 300)); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 2952 32
/// 2984 0
/// 832
/// ```
///
/// The declarations as Reflection renders them.
///
/// ```php
/// foreach (['sprintf', 'printf', 'vsprintf', 'vprintf', 'fprintf', 'vfprintf'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function sprintf ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $format ]
///     Parameter #1 [ <optional> mixed ...$values ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function printf ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $format ]
///     Parameter #1 [ <optional> mixed ...$values ]
///   }
///   - Return [ int ]
/// }
/// Function [ <internal:standard> function vsprintf ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $format ]
///     Parameter #1 [ <required> array $values ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function vprintf ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $format ]
///     Parameter #1 [ <required> array $values ]
///   }
///   - Return [ int ]
/// }
/// Function [ <internal:standard> function fprintf ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> $stream ]
///     Parameter #1 [ <required> string $format ]
///     Parameter #2 [ <optional> mixed ...$values ]
///   }
///   - Return [ int ]
/// }
/// Function [ <internal:standard> function vfprintf ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> $stream ]
///     Parameter #1 [ <required> string $format ]
///     Parameter #2 [ <required> array $values ]
///   }
///   - Return [ int ]
/// }
/// ```
#[php_function]
fn sprintf(
    req: &mut Request,
    format: PhpString,
    values: Variadic<Value>,
) -> Result<Owner<PhpString>, Thrown> {
    formatted(req, format.as_bytes(), &values.0, Some(1))
}

/// `printf(string $format, mixed ...$values): int`, php's
/// `PHP_FUNCTION(printf)`: `sprintf`'s bytes written where `echo`
/// writes them, `PHPWRITE`, and their count answered.
///
/// ```php
/// var_dump(printf("a%sb\n", "X"), printf(""), printf("%s", ""), printf("%05.1f\n", 3.14159));
/// try { printf(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// aXb
/// 003.1
/// int(4)
/// int(0)
/// int(0)
/// int(6)
/// printf() expects at least 1 argument, 0 given
/// ```
#[php_function]
fn printf(req: &mut Request, format: PhpString, values: Variadic<Value>) -> Result<i64, Thrown> {
    let formatted = formatted(req, format.as_bytes(), &values.0, Some(1))?;
    req.write(formatted.as_bytes());
    Ok(efree(req, formatted))
}

/// `vsprintf(string $format, array $values): string`, php's
/// `PHP_FUNCTION(vsprintf)`: the format over the array's values in
/// their order, keys ignored and a reference followed
/// (`php_formatted_print_get_array`), and a missing one the
/// `ValueError` php raises where the arguments came from an array.
///
/// ```php
/// $r = 5; $a = [5, 2, 5, 2]; $a[0] = &$r; $a[2] = &$r;
/// var_dump(vsprintf("%d-%s", ["x" => 5, "y" => "z"]), vsprintf("%d %d|%*d", $a), vsprintf("", []));
/// foreach ([["%3\$s", [1, 2]], ["%s %s", [1]], ["%s", "x"]] as $p) { try { vsprintf($p[0], $p[1]); } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; } }
/// try { vsprintf("%s"); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "5-z"
/// string(9) "5 2|    2"
/// string(0) ""
/// ValueError: The arguments array must contain 3 items, 2 given
/// ValueError: The arguments array must contain 2 items, 1 given
/// TypeError: vsprintf(): Argument #2 ($values) must be of type array, string given
/// vsprintf() expects exactly 2 arguments, 1 given
/// ```
#[php_function]
fn vsprintf(
    req: &mut Request,
    format: PhpString,
    values: PhpArray,
) -> Result<Owner<PhpString>, Thrown> {
    let (arguments, args) = values_of(req, values)?;
    let formatted = formatted(req, format.as_bytes(), &arguments, None);
    args.free(req);
    formatted
}

/// `php_formatted_print_get_array`: the array's values in their order,
/// keys ignored, beside the `safe_emalloc(n, sizeof(zval), 0)` php
/// copies them into, which stands across the format and its throws
/// until the caller's `efree(args)`.
fn values_of(req: &mut Request, array: PhpArray) -> Result<(Vec<Value>, Emalloc), Thrown> {
    let values: Vec<Value> = php_ext::values::walk::rows(req, array)
        .map(|(_, value)| value)
        .collect();
    let args = Emalloc::new(req, values.len() * 16)?;
    Ok((values, args))
}

/// `zend_string_efree(result)` after a printing caller wrote it: the
/// count it answers, `ZSTR_LEN(result)`.
fn efree(req: &mut Request, result: Owner<PhpString>) -> i64 {
    let len = i64::try_from(result.len()).unwrap_or(i64::MAX);
    result.give_up(req);
    len
}

/// `vprintf(string $format, array $values): int`, php's
/// `PHP_FUNCTION(vprintf)`: `vsprintf`'s bytes written where `echo`
/// writes them, and their count answered.
///
/// ```php
/// var_dump(vprintf("%s-%05.1f|%'*6s\n", ["a", 3.14159, "x"]), vprintf("", []));
/// foreach ([["%s %s", [1]], ["%s", "x"]] as $p) { try { vprintf($p[0], $p[1]); } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; } }
/// try { vprintf("%s"); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// a-003.1|*****x
/// int(15)
/// int(0)
/// ValueError: The arguments array must contain 2 items, 1 given
/// TypeError: vprintf(): Argument #2 ($values) must be of type array, string given
/// vprintf() expects exactly 2 arguments, 1 given
/// ```
#[php_function]
fn vprintf(req: &mut Request, format: PhpString, values: PhpArray) -> Result<i64, Thrown> {
    let (arguments, args) = values_of(req, values)?;
    let formatted = formatted(req, format.as_bytes(), &arguments, None);
    args.free(req);
    let formatted = formatted?;
    req.write(formatted.as_bytes());
    Ok(efree(req, formatted))
}

/// `fprintf($stream, string $format, mixed ...$values): int`, php's
/// `PHP_FUNCTION(fprintf)`: the stream first, then `sprintf`'s bytes
/// written to it with `php_stream_write`, and their count answered
/// whatever the write took; two parameters stand before the variadic
/// ones in the count its `ArgumentCountError` names.
///
/// ```php
/// $f = fopen("php://memory", "w+"); var_dump(fprintf($f, "%d-%s", 7, "x"), fprintf($f, "|%05.2f", 1.5)); rewind($f); var_dump(fread($f, 100));
/// var_dump(fprintf(fopen("php://output", "w"), "out %s\n", "ok"));
/// try { fprintf($f, "%s %s", 1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// try { fprintf("x", "y"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { fprintf($f); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// int(6)
/// string(9) "7-x|01.50"
/// out ok
/// int(7)
/// 4 arguments are required, 3 given
/// fprintf(): Argument #1 ($stream) must be of type resource, string given
/// fprintf() expects at least 2 arguments, 1 given
/// ```
///
/// A closed stream is refused where the parse reaches `$stream`, before
/// `$format` is read.
///
/// ```php
/// $f = fopen("php://memory", "w"); fclose($f);
/// try { fprintf($f, []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { vfprintf($f, "%s", 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// fprintf(): Argument #1 ($stream) must be an open stream resource
/// vfprintf(): Argument #1 ($stream) must be an open stream resource
/// ```
#[php_function]
fn fprintf(
    req: &mut Request,
    stream: Stream,
    format: Raw<PhpString>,
    values: Variadic<Value>,
) -> Result<i64, Thrown> {
    let format = format.hold(req)?;
    let formatted = formatted(req, format.as_bytes(), &values.0, Some(2))?;
    if !formatted.is_empty() {
        stream.write(req, formatted.as_bytes());
    }
    Ok(efree(req, formatted))
}

/// `vfprintf($stream, string $format, array $values): int`, php's
/// `PHP_FUNCTION(vfprintf)`: `vsprintf` into the stream.
///
/// ```php
/// $f = fopen("php://memory", "w+"); var_dump(vfprintf($f, "%2\$s %1\$s", ["a", "b"])); rewind($f); var_dump(fread($f, 100));
/// try { vfprintf($f, "%s %s", ["x"]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { vfprintf($f, "%s", "x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// string(3) "b a"
/// The arguments array must contain 2 items, 1 given
/// vfprintf(): Argument #3 ($values) must be of type array, string given
/// ```
#[php_function]
fn vfprintf(
    req: &mut Request,
    stream: Stream,
    format: PhpString,
    values: PhpArray,
) -> Result<i64, Thrown> {
    let (arguments, args) = values_of(req, values)?;
    let formatted = formatted(req, format.as_bytes(), &arguments, None);
    args.free(req);
    let formatted = formatted?;
    if !formatted.is_empty() {
        stream.write(req, formatted.as_bytes());
    }
    Ok(efree(req, formatted))
}

/// `INT_MAX`, the bound on an argnum, a width and a precision.
const INT_MAX: i64 = 2_147_483_647;
/// `FLOAT_PRECISION`: the places a double prints at with no precision.
const FLOAT_PRECISION: i64 = 6;
/// `MAX_FLOAT_PRECISION`: the most places a precision asks for.
const MAX_FLOAT_PRECISION: i64 = 53;

/// `ALIGN_LEFT` and `ALIGN_RIGHT`: which side of the field the padding
/// goes on, right unless `-` says left.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Right,
}

/// One specifier's modifiers, reset at each `%`.
#[derive(Clone, Copy)]
struct Spec {
    alignment: Align,
    /// The byte the field is padded with: a space, `0`, or the one after `'`.
    padding: u8,
    /// `+`: a sign before a number that has none.
    always_sign: bool,
    /// A precision was given with a number or `*`, php's `expprec`:
    /// what makes a precision the byte cap of `%s`, `%b`, `%o` and `%x`.
    expprec: bool,
    /// `ADJ_PRECISION`: a precision was given, with a number, `*` or
    /// nothing after the `.`; what keeps `%.f` at zero places where
    /// `%f` is six.
    precision_given: bool,
    width: i64,
    precision: i64,
}

impl Spec {
    /// "starting a new format specifier, reset variables".
    const fn reset() -> Spec {
        Spec {
            alignment: Align::Right,
            padding: b' ',
            always_sign: false,
            expprec: false,
            precision_given: false,
            width: 0,
            precision: 0,
        }
    }
}

/// What reading a specifier's modifiers came to: the spec with the
/// argnum its `n$` named, or the index of an argument a `*` reached
/// for and the call did not give, php's `continue` past the specifier.
enum Modifiers {
    Read(Spec, Option<usize>),
    Missing(usize),
}

/// `php_formatted_print(format, format_len, args, argc,
/// nb_additional_parameters)`: the bytes of the format with each
/// specifier replaced, or the `ValueError` a specifier is refused with,
/// or the count error for the argument the format reached past the
/// end of `args`, raised once the whole format has been read.
/// `additional` is `nb_additional_parameters`: the parameters before
/// the variadic ones counted into the `ArgumentCountError`, and none
/// for the `ValueError` the array natives raise instead.
fn formatted(
    req: &mut Request,
    format: &[u8],
    args: &[Value],
    additional: Option<usize>,
) -> Result<Owner<PhpString>, Thrown> {
    let mut out = Buffer::alloc(req)?;
    match format_into(req, &mut out, format, args, additional) {
        Ok(()) => out.finish(req),
        Err(thrown) => {
            out.free(req);
            Err(thrown)
        }
    }
}

/// The loop of `php_formatted_print` over a string already laid down,
/// so whatever it throws stands beside that string, which the caller
/// frees after it, php's `goto fail`.
fn format_into(
    req: &mut Request,
    out: &mut Buffer,
    format: &[u8],
    args: &[Value],
    additional: Option<usize>,
) -> Result<(), Thrown> {
    let mut at = 0;
    let mut currarg = 0usize;
    let mut max_missing: Option<usize> = None;
    while at < format.len() {
        let Some(percent) = format[at..].iter().position(|b| *b == b'%') else {
            out.chars(req, &format[at..])?;
            break;
        };
        if percent > 0 {
            out.chars(req, &format[at..at + percent])?;
        }
        at += percent + 1;
        if byte_at(format, at) == b'%' {
            out.char(req, b'%')?;
            at += 1;
            continue;
        }
        let (spec, argnum) = if byte_at(format, at).is_ascii_alphabetic() {
            (Spec::reset(), None)
        } else {
            match modifiers(req, format, &mut at, args, &mut currarg)? {
                Modifiers::Read(spec, argnum) => (spec, argnum),
                Modifiers::Missing(index) => {
                    max_missing = max_missing.max(Some(index));
                    continue;
                }
            }
        };
        if byte_at(format, at) == b'l' {
            at += 1;
        }
        let index = match argnum {
            Some(index) => index,
            None => next_argument(&mut currarg),
        };
        let Some(argument) = args.get(index) else {
            max_missing = max_missing.max(Some(index));
            continue;
        };
        let conversion = byte_at(format, at);
        if spec.expprec && spec.precision == -1 && !matches!(conversion, b'g' | b'G' | b'h' | b'H')
        {
            return Err(value_error(
                req,
                "Precision -1 is only supported for %g, %G, %h and %H",
            ));
        }
        if conversion == 0 && at >= format.len() {
            return Err(value_error(
                req,
                "Missing format specifier at end of string",
            ));
        }
        convert(req, out, &spec, conversion, argument)?;
        at += 1;
    }
    if let Some(missing) = max_missing {
        return Err(match additional {
            None => value_error(
                req,
                &format!(
                    "The arguments array must contain {} items, {} given",
                    missing + 1,
                    args.len()
                ),
            ),
            Some(additional) => throw_error(
                req,
                "ArgumentCountError",
                &format!(
                    "{} arguments are required, {} given",
                    missing + additional + 1,
                    args.len() + additional
                ),
            ),
        });
    }
    Ok(())
}

/// The byte at `at`, and the terminator past the end: `ZSTR_VAL` is
/// NUL-terminated, and php's parser reads that NUL as one more byte.
fn byte_at(format: &[u8], at: usize) -> u8 {
    format.get(at).copied().unwrap_or(0)
}

/// The specifier from its `%` to its conversion, "first look for
/// argnum", "after argnum comes modifiers", "after modifiers comes
/// width", "after width and argnum comes precision": each read where
/// php reads it, `at` left on the conversion, and a `*` that reaches
/// past the arguments given the `Missing` php `continue`s on.
fn modifiers(
    req: &mut Request,
    format: &[u8],
    at: &mut usize,
    args: &[Value],
    currarg: &mut usize,
) -> Result<Modifiers, Thrown> {
    let mut spec = Spec::reset();
    let argnum = get_argnum(req, format, at)?;
    loop {
        match byte_at(format, *at) {
            b' ' | b'0' => spec.padding = byte_at(format, *at),
            b'-' => spec.alignment = Align::Left,
            b'+' => spec.always_sign = true,
            b'\'' => {
                if format.len() - *at > 1 {
                    *at += 1;
                    spec.padding = byte_at(format, *at);
                } else {
                    return Err(value_error(req, "Missing padding character"));
                }
            }
            _ => break,
        }
        *at += 1;
    }
    if byte_at(format, *at) == b'*' {
        *at += 1;
        let index = match get_argnum(req, format, at)? {
            Some(index) => index,
            None => next_argument(currarg),
        };
        let Some(argument) = args.get(index) else {
            return Ok(Modifiers::Missing(index));
        };
        let Some(width) = argument.deref().as_int() else {
            return Err(value_error(req, "Width must be an integer"));
        };
        if !(0..=INT_MAX).contains(&width) {
            return Err(value_error(
                req,
                &format!("Width must be between 0 and {INT_MAX}"),
            ));
        }
        spec.width = width;
    } else if byte_at(format, *at).is_ascii_digit() {
        spec.width = get_number(format, at);
        if spec.width < 0 {
            return Err(value_error(
                req,
                &format!("Width must be between 0 and {INT_MAX}"),
            ));
        }
    }
    if byte_at(format, *at) == b'.' {
        *at += 1;
        spec.precision_given = true;
        if byte_at(format, *at) == b'*' {
            *at += 1;
            let index = match get_argnum(req, format, at)? {
                Some(index) => index,
                None => next_argument(currarg),
            };
            let Some(argument) = args.get(index) else {
                return Ok(Modifiers::Missing(index));
            };
            let Some(precision) = argument.deref().as_int() else {
                return Err(value_error(req, "Precision must be an integer"));
            };
            if !(-1..=INT_MAX).contains(&precision) {
                return Err(value_error(
                    req,
                    &format!("Precision must be between -1 and {INT_MAX}"),
                ));
            }
            spec.precision = precision;
            spec.expprec = true;
        } else if byte_at(format, *at).is_ascii_digit() {
            spec.precision = get_number(format, at);
            if spec.precision < 0 {
                return Err(value_error(
                    req,
                    &format!("Precision must be between 0 and {INT_MAX}"),
                ));
            }
            spec.expprec = true;
        }
    }
    Ok(Modifiers::Read(spec, argnum))
}

/// "now we expect to find a type specifier": one conversion of
/// `argument` appended, or the `ValueError` for a byte that is none.
fn convert(
    req: &mut Request,
    out: &mut Buffer,
    spec: &Spec,
    conversion: u8,
    argument: &Value,
) -> Result<(), Thrown> {
    match conversion {
        b's' => {
            let string = to_string(req, argument)?;
            let appended = append_string_at(
                req,
                out,
                string.as_bytes(),
                spec.width,
                usize::try_from(spec.precision).unwrap_or(0),
                spec,
                Sign::None,
                spec.expprec,
            );
            string.give_up(req);
            appended
        }
        b'd' => {
            let number = to_long(req, argument)?;
            append_int(req, out, number, spec)
        }
        b'u' => {
            let number = to_long(req, argument)?;
            append_uint(req, out, number, spec)
        }
        b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'h' | b'H' => {
            let number = to_double(req, argument)?;
            append_double(req, out, number, spec, conversion)
        }
        b'c' => {
            let number = to_long(req, argument)?;
            out.char(req, (number & 0xff).to_le_bytes()[0])
        }
        b'o' | b'x' | b'X' | b'b' => {
            let number = to_long(req, argument)?;
            let bits = match conversion {
                b'o' => 3,
                b'b' => 1,
                _ => 4,
            };
            append_2n(req, out, number, spec, bits, conversion == b'X')
        }
        b'%' => out.char(req, b'%'),
        other => {
            // `zend_value_error("Unknown format specifier \"%c\"", *format)`,
            // which `zend_throw_exception` then reads as a C string, so
            // a NUL ends the message before its closing quote.
            let message = if other == 0 {
                "Unknown format specifier \"".to_owned()
            } else {
                format!("Unknown format specifier \"{}\"", char::from(other))
            };
            Err(value_error(req, &message))
        }
    }
}

/// `currarg++`: the argument a specifier with no argnum takes, and the
/// next one after it for the next such specifier.
fn next_argument(currarg: &mut usize) -> usize {
    let taken = *currarg;
    *currarg += 1;
    taken
}

/// `zend_value_error`: a `ValueError` with the message as it is.
fn value_error(req: &mut Request, message: &str) -> Thrown {
    throw_error(req, "ValueError", message)
}

/// `php_sprintf_getnumber`: `ZEND_STRTOL` over the digits at `at`,
/// which moves past them, and `-1` for a number at `INT_MAX` or past
/// it, where the long overflowed.
fn get_number(format: &[u8], at: &mut usize) -> i64 {
    let mut number: i64 = 0;
    while let Some(digit) = format.get(*at).filter(|b| b.is_ascii_digit()) {
        number = number
            .saturating_mul(10)
            .saturating_add(i64::from(digit - b'0'));
        *at += 1;
    }
    if number >= INT_MAX { -1 } else { number }
}

/// `php_sprintf_get_argnum`: the digits before a `$` as a zero-based
/// argument index, `None` for `ARG_NUM_NEXT` where no `$` follows the
/// digits, and php's `ValueError` for a number that is zero or too wide.
fn get_argnum(req: &mut Request, format: &[u8], at: &mut usize) -> Result<Option<usize>, Thrown> {
    let mut probe = *at;
    while format.get(probe).is_some_and(u8::is_ascii_digit) {
        probe += 1;
    }
    if format.get(probe) != Some(&b'$') {
        return Ok(None);
    }
    let argnum = get_number(format, at);
    if argnum <= 0 {
        return Err(value_error(
            req,
            &format!("Argument number specifier must be greater than zero and less than {INT_MAX}"),
        ));
    }
    *at += 1;
    Ok(Some(usize::try_from(argnum - 1).unwrap_or(0)))
}

/// What a number's first byte is, for the appender's zero-padding arm,
/// which writes the sign before the padding and skips it in the text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sign {
    None,
    Negative,
    Positive,
}

/// `php_sprintf_appendstring(buffer, pos, add, min_width, max_width,
/// padding, alignment, len, neg, expprec, always_sign)` at the spec's
/// width and with no `max_width`, which every caller but `%s` passes as
/// zero: `add` cut to nothing where `expprec` says so, padded to the
/// width on the side the alignment says, with a negative or an
/// always-written sign moved before zero padding. That zero is why a
/// precision empties `%x`, `%o` and `%b`, as php's does.
fn append_string(
    req: &mut Request,
    out: &mut Buffer,
    add: &[u8],
    spec: &Spec,
    sign: Sign,
    expprec: bool,
) -> Result<(), Thrown> {
    append_string_at(req, out, add, spec.width, 0, spec, sign, expprec)
}

/// The appender with `min_width` and `max_width` handed in: the
/// non-finite doubles pass their own length for the first and `%s` its
/// precision for the second. A field the buffer cannot hold below
/// `INT_MAX` is php's `E_ERROR`.
#[expect(
    clippy::too_many_arguments,
    reason = "php_sprintf_appendstring's own, one per parameter it reads"
)]
fn append_string_at(
    req: &mut Request,
    out: &mut Buffer,
    add: &[u8],
    min_width: i64,
    max_width: usize,
    spec: &Spec,
    sign: Sign,
    expprec: bool,
) -> Result<(), Thrown> {
    let min_width = usize::try_from(min_width).unwrap_or(0);
    let mut copy_len = if expprec {
        max_width.min(add.len())
    } else {
        add.len()
    };
    let npad = min_width.saturating_sub(copy_len);
    let m_width = min_width.max(copy_len);
    let int_max = usize::try_from(INT_MAX).unwrap_or(usize::MAX);
    if m_width > int_max.saturating_sub(out.pos).saturating_sub(1) {
        return Err(fatal(req, &format!("Field width {m_width} is too long")));
    }
    let req_size = out.pos + m_width + 1;
    if req_size > out.len() {
        let mut size = out.len();
        while req_size > size {
            if size > usize::MAX / 2 {
                return Err(fatal(req, &format!("Field width {req_size} is too long")));
            }
            size <<= 1;
        }
        out.extend(req, size)?;
    }
    let mut add = add;
    if spec.alignment == Align::Right {
        if sign != Sign::None && spec.padding == b'0' && copy_len > 0 {
            out.put(&[if sign == Sign::Negative { b'-' } else { b'+' }]);
            add = &add[1..];
            copy_len -= 1;
        }
        out.pad(spec.padding, npad);
    }
    out.put(&add[..copy_len]);
    if spec.alignment == Align::Left {
        out.pad(spec.padding, npad);
    }
    Ok(())
}

/// `php_formatted_print`'s `result`: `zend_string_alloc(240, 0)` laid
/// down before the format is read, `outpos` into it, grown by
/// `zend_string_extend` to a doubling of its length where an appender
/// needs more, and handed out at its length with its allocation as it
/// stands, as php never cuts it.
struct Buffer {
    result: Option<FreshString>,
    /// `outpos`: the bytes written so far.
    pos: usize,
}

impl Buffer {
    /// `result = zend_string_alloc(size, 0)` with `size = 240`.
    fn alloc(req: &mut Request) -> Result<Buffer, Thrown> {
        Ok(Buffer {
            result: Some(FreshString::new(req, 240)?),
            pos: 0,
        })
    }

    /// `ZSTR_LEN(result)`: the bytes the allocation holds.
    fn len(&self) -> usize {
        self.result.as_ref().map_or(0, FreshString::len)
    }

    /// `result = zend_string_extend(result, size, 0)`; a memory limit
    /// met leaves the string the request's end's.
    fn extend(&mut self, req: &mut Request, size: usize) -> Result<(), Thrown> {
        if let Some(result) = self.result.take() {
            self.result = Some(result.extend(req, size)?);
        }
        Ok(())
    }

    /// `php_sprintf_appendchar`: one byte, the length doubled first
    /// where it and the NUL would not fit.
    fn char(&mut self, req: &mut Request, add: u8) -> Result<(), Thrown> {
        if self.pos + 1 >= self.len() {
            self.extend(req, self.len() << 1)?;
        }
        self.put(&[add]);
        Ok(())
    }

    /// `php_sprintf_appendchars`: the bytes, the length doubled until
    /// they and the NUL fit.
    fn chars(&mut self, req: &mut Request, add: &[u8]) -> Result<(), Thrown> {
        if self.pos + add.len() >= self.len() {
            let mut size = self.len();
            while self.pos + add.len() >= size {
                size <<= 1;
            }
            self.extend(req, size)?;
        }
        self.put(add);
        Ok(())
    }

    /// The bytes written at `outpos`, where the appender made room.
    fn put(&mut self, add: &[u8]) {
        if let Some(result) = self.result.as_mut()
            && let Some(at) = result.bytes_mut().get_mut(self.pos..self.pos + add.len())
        {
            at.copy_from_slice(add);
        }
        self.pos += add.len();
    }

    /// `npad` padding bytes at `outpos`.
    fn pad(&mut self, padding: u8, npad: usize) {
        if let Some(result) = self.result.as_mut()
            && let Some(at) = result.bytes_mut().get_mut(self.pos..self.pos + npad)
        {
            at.fill(padding);
        }
        self.pos += npad;
    }

    /// `ZSTR_LEN(result) = outpos` and the string handed out; one an
    /// extension lost to the memory limit never reaches here.
    fn finish(self, req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
        match self.result {
            Some(mut result) => {
                result.set_len(self.pos);
                Ok(result.finish())
            }
            None => PhpString::new(req, b""),
        }
    }

    /// `fail: zend_string_efree(result)`.
    fn free(self, req: &mut Request) {
        if let Some(result) = self.result {
            result.free(req);
        }
    }
}

/// `php_sprintf_appendint`: the long's decimal with its sign, or a `+`
/// where `always_sign` asks for one, and zero padding turned to spaces
/// where the alignment is left, since php cannot right-pad an integer
/// with zeros.
fn append_int(req: &mut Request, out: &mut Buffer, number: i64, spec: &Spec) -> Result<(), Thrown> {
    let (text, sign) = if number < 0 {
        (Decimal::of(number), Sign::Negative)
    } else if spec.always_sign {
        (Decimal::plus(number), Sign::Positive)
    } else {
        (Decimal::of(number), Sign::None)
    };
    let spec = integer_padding(spec);
    append_string(req, out, text.as_bytes(), &spec, sign, false)
}

/// `php_sprintf_appenduint`: the long's bits as an unsigned decimal.
fn append_uint(
    req: &mut Request,
    out: &mut Buffer,
    number: i64,
    spec: &Spec,
) -> Result<(), Thrown> {
    let text = Decimal::unsigned(u64::from_ne_bytes(number.to_ne_bytes()));
    let spec = integer_padding(spec);
    append_string(req, out, text.as_bytes(), &spec, Sign::None, false)
}

/// "Can't right-pad 0's on integers": the spec with a left-aligned `0`
/// padding read as a space.
fn integer_padding(spec: &Spec) -> Spec {
    Spec {
        padding: if spec.alignment == Align::Left && spec.padding == b'0' {
            b' '
        } else {
            spec.padding
        },
        ..*spec
    }
}

/// `php_sprintf_append2n`: the long's bits in groups of `n`, each a
/// digit of the table, with the precision as the byte cap where one
/// was given.
fn append_2n(
    req: &mut Request,
    out: &mut Buffer,
    number: i64,
    spec: &Spec,
    n: u32,
    upper: bool,
) -> Result<(), Thrown> {
    let table: &[u8; 16] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut num = u64::from_ne_bytes(number.to_ne_bytes());
    let andbits = (1u64 << n) - 1;
    let mut digits = Vec::new();
    loop {
        digits.push(table[usize::try_from(num & andbits).unwrap_or(0)]);
        num >>= n;
        if num == 0 {
            break;
        }
    }
    digits.reverse();
    append_string(req, out, &digits, spec, Sign::None, spec.expprec)
}

/// `php_sprintf_appenddouble(buffer, pos, number, width, padding,
/// alignment, precision, adjust, fmt, always_sign)`: six places unless
/// a precision was given, capped at 53 behind the notice; `NaN`, `INF`
/// and `-INF` at their own width; `e`, `E`, `f` and `F` over
/// `php_conv_fp`, and `g`, `G`, `h` and `H` over `zend_gcvt` with a
/// zero precision read as one.
fn append_double(
    req: &mut Request,
    out: &mut Buffer,
    number: f64,
    spec: &Spec,
    fmt: u8,
) -> Result<(), Thrown> {
    let mut precision = spec.precision;
    if !spec.precision_given {
        precision = FLOAT_PRECISION;
    } else if precision > MAX_FLOAT_PRECISION {
        docref(
            req,
            Level::Notice,
            &format!(
                "Requested precision of {precision} digits was truncated to PHP maximum of {MAX_FLOAT_PRECISION} digits"
            ),
        );
        precision = MAX_FLOAT_PRECISION;
    }
    if number.is_nan() {
        // `is_negative = (number < 0)`, which no NaN is.
        return append_string_at(req, out, b"NaN", 3, 0, spec, sign_of(false, spec), false);
    }
    if number.is_infinite() {
        let negative = number < 0.0;
        let text: &[u8] = if negative { b"-INF" } else { b"INF" };
        let width = i64::try_from(text.len()).unwrap_or(0);
        return append_string_at(
            req,
            out,
            text,
            width,
            0,
            spec,
            sign_of(negative, spec),
            false,
        );
    }
    let (text, negative) = match fmt {
        b'e' | b'E' | b'f' | b'F' => {
            let (digits, negative) = conv_fp(fmt, number, precision);
            let signed = if negative {
                format!("-{digits}")
            } else if spec.always_sign {
                format!("+{digits}")
            } else {
                digits
            };
            (signed, negative)
        }
        _ => {
            let precision = if precision == 0 { 1 } else { precision };
            let exponent = if matches!(fmt, b'G' | b'H') {
                Exponent::Upper
            } else {
                Exponent::Lower
            };
            let mut text = float_format(
                number,
                i32::try_from(precision).unwrap_or(i32::MAX),
                exponent,
            );
            let negative = text.starts_with('-');
            if !negative && spec.always_sign {
                text.insert(0, '+');
            }
            (text, negative)
        }
    };
    // `LCONV_DECIMAL_POINT` for `f`, `g` and `G`, the request's
    // `LC_NUMERIC` radix read off the thread it is installed on; `e`,
    // `E`, `F`, `h` and `H` keep `.` under every locale.
    let mut text = text.into_bytes();
    if matches!(fmt, b'f' | b'g' | b'G')
        && let Some(point) = text.iter_mut().find(|b| **b == b'.')
    {
        *point = crate::ffi::locale::decimal_point();
    }
    append_string(req, out, &text, spec, sign_of(negative, spec), false)
}

/// The sign the appender moves before zero padding: the number's own
/// where it is negative, and the `+` `always_sign` wrote otherwise.
fn sign_of(negative: bool, spec: &Spec) -> Sign {
    if negative {
        Sign::Negative
    } else if spec.always_sign {
        Sign::Positive
    } else {
        Sign::None
    }
}

/// `php_conv_fp(format, num, add_dp = false, precision, dec_point,
/// is_negative, buf, len)` (`Z/main/snprintf.c:271`) for a finite
/// double: `php_fcvt` under `F`, the digits to `precision` places
/// after the point, and `php_ecvt` under `e` and `E`, one digit, the
/// point where the precision is not zero, `precision` more digits, and
/// the exponent with its sign and no padding; both `zend_dtoa`'s
/// correctly rounded digits, which Rust's formatter gives too. The
/// sign is answered beside the text, since a zero is never negative
/// here (`__cvt` clears it), where `zend_gcvt` keeps `-0`.
fn conv_fp(fmt: u8, number: f64, precision: i64) -> (String, bool) {
    let places = usize::try_from(precision).unwrap_or(0);
    let negative = number != 0.0 && number < 0.0;
    let magnitude = number.abs();
    if matches!(fmt, b'f' | b'F') {
        return (format!("{magnitude:.places$}"), negative);
    }
    let rendered = format!("{magnitude:.places$e}");
    let (mantissa, exponent) = rendered.split_once('e').unwrap_or((rendered.as_str(), "0"));
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let mut text = String::from(mantissa);
    text.push(char::from(fmt));
    text.push(if exponent < 0 { '-' } else { '+' });
    text.push_str(Decimal::unsigned(exponent.unsigned_abs()).as_str());
    (text, negative)
}
