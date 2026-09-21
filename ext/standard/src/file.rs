//! The first of `file.c`'s natives (`Z/ext/standard/file.c`), the slice
//! that makes a stream resource and moves bytes through it: `rewind`,
//! `fclose`, `feof`, `fread`, `fopen`, `ftell`, `fwrite` and its alias
//! `fputs`, in the stub's order (`basic_functions.stub.php:2738-2815`),
//! each over the engine's streams facet (design §4.3 seam 5), which is
//! where `php_stream_open_wrapper_ex`, `php_stream_read` and their
//! siblings live. A `$stream` parameter is `PHP_Z_PARAM_STREAM`: a
//! resource, and one that is an open stream.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::coerce::{coerce_param, value_name};
use php_ext::decl::{ConstExpr, FunctionDecl, Parameter, Type};
use php_ext::raising::{Thrown, arg_type_error, arg_value_error};
use php_ext::request::{Request, Stream, Whence};
use php_ext::values::{Value, ValueRef};

use crate::output::Entry;
use crate::{
    argument_count_between, bool_argument, declared, long_argument, parameter, string_argument,
};

/// The natives declared here, with the untyped `$stream` the stub
/// declares and `fopen`'s return left undeclared, as php declares
/// them, so `ReflectionFunction::__toString` renders each as the oracle
/// does.
///
/// ```php
/// echo new ReflectionFunction('fopen'), new ReflectionFunction('fwrite'), new ReflectionFunction('fputs');
/// ```
/// ```expect
/// Function [ <internal:standard> function fopen ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> string $filename ]
///     Parameter #1 [ <required> string $mode ]
///     Parameter #2 [ <optional> bool $use_include_path = false ]
///     Parameter #3 [ <optional> $context = null ]
///   }
/// }
/// Function [ <internal:standard> function fwrite ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> $stream ]
///     Parameter #1 [ <required> string $data ]
///     Parameter #2 [ <optional> ?int $length = null ]
///   }
///   - Return [ int|false ]
/// }
/// Function [ <internal:standard> function fputs ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> $stream ]
///     Parameter #1 [ <required> string $data ]
///     Parameter #2 [ <optional> ?int $length = null ]
///   }
///   - Return [ int|false ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let stream = || Parameter {
        ty: None,
        ..parameter(b"stream", Type::Mixed)
    };
    let fwrite_decl = |name: &[u8]| {
        declared(
            name,
            &[
                stream(),
                parameter(b"data", Type::String),
                Parameter {
                    default: Some(ConstExpr::Null),
                    ..parameter(b"length", Type::Nullable(Box::new(Type::Int)))
                },
            ],
            Type::Union(vec![Type::Int, Type::False]),
        )
    };
    vec![
        (
            declared(b"rewind", &[stream()], Type::Bool),
            rewind as Entry,
        ),
        (declared(b"fclose", &[stream()], Type::Bool), fclose),
        (declared(b"feof", &[stream()], Type::Bool), feof),
        (
            declared(
                b"fread",
                &[stream(), parameter(b"length", Type::Int)],
                Type::Union(vec![Type::String, Type::False]),
            ),
            fread,
        ),
        (
            FunctionDecl {
                return_type: None,
                ..declared(
                    b"fopen",
                    &[
                        parameter(b"filename", Type::String),
                        parameter(b"mode", Type::String),
                        Parameter {
                            default: Some(ConstExpr::Bool(false)),
                            ..parameter(b"use_include_path", Type::Bool)
                        },
                        Parameter {
                            ty: None,
                            default: Some(ConstExpr::Null),
                            ..parameter(b"context", Type::Mixed)
                        },
                    ],
                    Type::Mixed,
                )
            },
            fopen,
        ),
        (
            declared(
                b"ftell",
                &[stream()],
                Type::Union(vec![Type::Int, Type::False]),
            ),
            ftell,
        ),
        (fwrite_decl(b"fwrite"), fwrite),
        (fwrite_decl(b"fputs"), fwrite),
    ]
}

/// `PHP_Z_PARAM_STREAM` (`Z/main/php_streams.h:322`) over
/// `php_stream_zend_parse_arg_into_stream`: the argument at `position`
/// as the stream it names; a resource of any other type, a closed one
/// included, is php's `must be an open stream resource`, and anything
/// that is not a resource is the parser's own `must be of type
/// resource, %s given`, no null deprecation and no coercion either
/// way.
fn stream_argument(req: &mut Request, position: u32) -> Result<Stream, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let given = given.deref();
    if let ValueRef::Resource(_) = given.as_ref() {
        return Stream::from_value(req, &given)
            .ok_or_else(|| arg_type_error(req, position, "be an open stream resource"));
    }
    let name = value_name(req, &given);
    Err(arg_type_error(
        req,
        position,
        &format!("be of type resource, {name} given"),
    ))
}

/// `rewind($stream): bool`, `PHP_FUNCTION(rewind)`
/// (`Z/ext/standard/file.c:1027`): `php_stream_rewind`, a seek to zero,
/// and whether it was taken. On `php://memory` the seek clears the eof
/// the last read set, so the same bytes read again; the position the
/// generic layer keeps is the seek's and then the write's, two, where
/// the memory kind's own is four.
///
/// ```php
/// $f = fopen('php://memory', 'r+');
/// var_dump(fwrite($f, "ab"), fread($f, 2), feof($f), rewind($f), feof($f), fread($f, 2), rewind($f), fwrite($f, "cd"), ftell($f), rewind($f), fread($f, 10));
/// ```
/// ```expect
/// int(2)
/// string(0) ""
/// bool(true)
/// bool(true)
/// bool(false)
/// string(2) "ab"
/// bool(true)
/// int(2)
/// int(2)
/// bool(true)
/// string(2) "cd"
/// ```
fn rewind(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let stream = stream_argument(req, 1)?;
    Ok(Value::bool(stream.seek(req, 0, Whence::Set)))
}

/// `fclose($stream): bool`, `PHP_FUNCTION(fclose)`
/// (`Z/ext/standard/file.c:752`): `php_stream_free` under
/// `PHP_STREAM_FREE_KEEP_RSRC | PHP_STREAM_FREE_CLOSE`, which closes the
/// resource and leaves it in the list for its owners, and `true`. What
/// is left is a closed resource: `var_dump` prints php's `Unknown` for
/// its type, `is_resource` says no, and a second `fclose` is the
/// `TypeError` `PHP_Z_PARAM_STREAM` raises for a resource that is not an
/// open stream, beside the parser's own wording for what is no resource.
///
/// ```php
/// $f = fopen('php://memory', 'r+');
/// var_dump(fclose($f), $f, is_resource($f));
/// try { fclose($f); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { fclose("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { fclose(null); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { fclose(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// resource(%d) of type (Unknown)
/// bool(false)
/// fclose(): Argument #1 ($stream) must be an open stream resource
/// fclose(): Argument #1 ($stream) must be of type resource, string given
/// fclose(): Argument #1 ($stream) must be of type resource, null given
/// fclose() expects exactly 1 argument, 0 given
/// ```
///
/// A `php://temp` is two resources, the stream and the memory stream it
/// encloses, as `_php_stream_temp_create_ex` registers them, so the
/// next resource's id is two further on; closing the temp closes and
/// frees the inner one with it.
///
/// ```php
/// $a = fopen('php://temp', 'r+'); $b = fopen('php://memory', 'r+');
/// var_dump((int)$b - (int)$a, fwrite($a, "temp!"), rewind($a), fread($a, 10), feof($a), fclose($a), $a);
/// ```
/// ```expect
/// int(2)
/// int(5)
/// bool(true)
/// string(5) "temp!"
/// bool(true)
/// bool(true)
/// resource(%d) of type (Unknown)
/// ```
fn fclose(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let stream = stream_argument(req, 1)?;
    stream.close(req);
    Ok(Value::bool(true))
}

/// `feof($stream): bool`, `PHP_FUNCTION(feof)`
/// (`Z/ext/standard/file.c:845`): `php_stream_eof`, which is never true
/// while the read buffer holds a byte and otherwise what the kind's
/// last read said. A plain file is read a chunk at a time, so after a
/// short file is read whole in two calls eof is still clear, and
/// it is the read that finds nothing that sets it; `php://memory` sets
/// it on the read that reaches its end, and only a read asking for
/// more than is there reaches it, since a read satisfied in full stops
/// before the kind is asked again.
///
/// ```php
/// $w = fopen(__DIR__ . '/feof.txt', 'w'); fwrite($w, "hello"); fclose($w);
/// $r = fopen(__DIR__ . '/feof.txt', 'r');
/// var_dump(feof($r), fread($r, 3), feof($r), fread($r, 2), feof($r), fread($r, 1), feof($r), fclose($r));
/// $r = fopen(__DIR__ . '/feof.txt', 'r');
/// var_dump(fread($r, 10), feof($r), rewind($r), feof($r), fread($r, 2));
/// $m = fopen('php://memory', 'r+'); fwrite($m, "hello"); rewind($m);
/// var_dump(feof($m), fread($m, 5), feof($m), rewind($m), fread($m, 3), feof($m), fread($m, 3), feof($m));
/// ```
/// ```expect
/// bool(false)
/// string(3) "hel"
/// bool(false)
/// string(2) "lo"
/// bool(false)
/// string(0) ""
/// bool(true)
/// bool(true)
/// string(5) "hello"
/// bool(true)
/// bool(true)
/// bool(false)
/// string(2) "he"
/// bool(false)
/// string(5) "hello"
/// bool(false)
/// bool(true)
/// string(3) "hel"
/// bool(false)
/// string(2) "lo"
/// bool(true)
/// ```
fn feof(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let stream = stream_argument(req, 1)?;
    Ok(Value::bool(stream.eof(req)))
}

/// `fread($stream, int $length): string|false`, `PHP_FUNCTION(fread)`
/// (`Z/ext/standard/file.c:1539`): a length at or below zero is php's
/// `ValueError`; then `php_stream_read_to_str`, up to `$length` bytes
/// and fewer at the end, and `false` for the read the kind refused,
/// which a file opened for writing alone does with php's notice and
/// `errno` in it.
///
/// ```php
/// $w = fopen(__DIR__ . '/fread.txt', 'w');
/// var_dump(fwrite($w, "hello"), fread($w, 3), feof($w), fclose($w));
/// $m = fopen('php://memory', 'r+'); fwrite($m, "hello"); rewind($m);
/// var_dump(fread($m, 2), fread($m, 100), fread($m, 1));
/// try { fread($m, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { fread($m, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { fread("x", 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Notice: fread(): Read of 8192 bytes failed with errno=9 Bad file descriptor in %s on line 3
/// int(5)
/// bool(false)
/// bool(false)
/// bool(true)
/// string(2) "he"
/// string(3) "llo"
/// string(0) ""
/// fread(): Argument #2 ($length) must be greater than 0
/// fread(): Argument #2 ($length) must be greater than 0
/// fread(): Argument #1 ($stream) must be of type resource, string given
/// ```
///
/// The string is asked for at `$length` before a byte is read,
/// `zend_string_alloc`, so a length the memory limit cannot hold is
/// php's fatal and not a read.
///
/// ```php
/// var_dump(fread(fopen('php://memory', 'r'), PHP_INT_MAX));
/// ```
/// ```expect exit=255
/// Fatal error: Allowed memory size of 134217728 bytes exhausted (tried to allocate %d bytes) in %s on line 2
/// ```
fn fread(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let stream = stream_argument(req, 1)?;
    let len = long_argument(req, 2)?;
    if len <= 0 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    Ok(stream
        .read(req, len)?
        .map_or_else(|| Value::bool(false), Value::string))
}

/// `fopen(string $filename, string $mode, bool $use_include_path = false, $context = null)`,
/// `PHP_FUNCTION(fopen)` (`Z/ext/standard/file.c:722`):
/// `php_stream_open_wrapper_ex` under `REPORT_ERRORS`, with `USE_PATH`
/// where asked, and the stream as a resource, or `false` after the
/// warning the open left. `$filename` is `Z_PARAM_PATH`, a string with
/// no null byte in it. `$context` is taken and not read: no wrapper
/// here reads one.
///
/// ```php
/// $f = fopen('php://memory', 'r+');
/// var_dump($f, fwrite($f, "hello"), ftell($f), fread($f, 3), feof($f), rewind($f), fread($f, 3), feof($f), fread($f, 10), feof($f), fread($f, 1), feof($f));
/// ```
/// ```expect
/// resource(%d) of type (stream)
/// int(5)
/// int(5)
/// string(0) ""
/// bool(true)
/// bool(true)
/// string(3) "hel"
/// bool(false)
/// string(2) "lo"
/// bool(true)
/// string(0) ""
/// bool(true)
/// ```
///
/// A plain file is opened through `php_stream_parse_fopen_modes`'s
/// `open(2)` flags: `w` creates and truncates, `a` appends, `r` needs
/// the file there, and a `$mode` php does not know is the wrapper's own
/// line after `Failed to open stream: `. The path is against the
/// working directory, and `file://` names the same wrapper.
///
/// ```php
/// $p = __DIR__ . '/fopen.txt';
/// $w = fopen($p, 'w'); var_dump(fwrite($w, "one"), fclose($w));
/// $a = fopen($p, 'a'); var_dump(fwrite($a, "two"), fclose($a));
/// $r = fopen('file://' . $p, 'r'); var_dump(fread($r, 100), fclose($r));
/// var_dump(fopen($p, 'q'), fopen(__DIR__ . '/missing.txt', 'r'));
/// try { fopen("a\0b", 'r'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { fopen('', 'r'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// bool(true)
/// int(3)
/// bool(true)
/// string(6) "onetwo"
/// bool(true)
///
/// Warning: fopen(%s/fopen.txt): Failed to open stream: `q' is not a valid mode for fopen in %s on line 6
///
/// Warning: fopen(%s/missing.txt): Failed to open stream: No such file or directory in %s on line 6
/// bool(false)
/// bool(false)
/// fopen(): Argument #1 ($filename) must not contain any null bytes
/// Path must not be empty
/// ```
///
/// `php://stdout` and `php://stderr` are the host's two fds, past the
/// `php_output` layer as php's CLI writes them; `php://output` is that
/// layer itself, so it is what `ob_start` catches and `php://stdout` is
/// not.
///
/// ```php
/// ob_start(); var_dump(fwrite(fopen('php://stdout', 'w'), "out\n"), fwrite(fopen('php://output', 'w'), "output\n")); $held = ob_get_clean(); echo "[", $held, "]";
/// var_dump(fwrite(fopen('php://stderr', 'w'), "err\n"));
/// ```
/// ```expect
/// out
/// [output
/// int(4)
/// int(7)
/// ]int(4)
/// ```
/// ```stderr
/// err
/// ```
fn fopen(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 4)?;
    let filename = string_argument(req, 1)?;
    if filename.as_bytes().contains(&0) {
        return Err(arg_value_error(req, 1, "not contain any null bytes"));
    }
    let mode = string_argument(req, 2)?;
    let use_include_path = if req.argument_count() >= 3 {
        bool_argument(req, 3)?
    } else {
        false
    };
    let opened = req.open_stream(filename.as_bytes(), mode.as_bytes(), use_include_path)?;
    Ok(opened.map_or_else(|| Value::bool(false), Stream::as_value))
}

/// `ftell($stream): int|false`, `PHP_FUNCTION(ftell)`
/// (`Z/ext/standard/file.c:1049`): `php_stream_tell`, the position the
/// generic layer keeps, and `false` where it is `-1`, which a stream
/// that cannot seek carries from its open. A file opened for append
/// starts at the offset `lseek(SEEK_CUR)` answers, which is zero until
/// the first write, so what `ftell` says after that write is what was
/// written and not the file's size; php reads the same offset.
///
/// ```php
/// $p = __DIR__ . '/ftell.txt';
/// $w = fopen($p, 'w'); fwrite($w, "hello"); fclose($w);
/// $a = fopen($p, 'a+'); var_dump(ftell($a), fwrite($a, "!!"), ftell($a), rewind($a), fread($a, 100), ftell($a), fclose($a));
/// var_dump(ftell(fopen('php://stdout', 'w')));
/// ```
/// ```expect
/// int(0)
/// int(2)
/// int(2)
/// bool(true)
/// string(7) "hello!!"
/// int(7)
/// bool(true)
/// bool(false)
/// ```
fn ftell(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let stream = stream_argument(req, 1)?;
    let at = stream.tell(req);
    Ok(if at == -1 {
        Value::bool(false)
    } else {
        Value::int(at)
    })
}

/// `fwrite($stream, string $data, ?int $length = null): int|false`,
/// `PHP_FUNCTION(fwrite)` (`Z/ext/standard/file.c:970`) and its alias
/// `fputs`: the bytes to write are `$data` whole, or the first
/// `$length` of them, or none for a length at or below zero, and none
/// is `0` before the stream is asked; then `php_stream_write`, how
/// many the stream took, and `false` for the `-1` a stream that does
/// not take them answers, `php://memory` opened `r` silently and a
/// file opened `r` with php's notice.
///
/// ```php
/// $m = fopen('php://memory', 'r+');
/// var_dump(fwrite($m, "abc", -1), fwrite($m, "abc", 2), fwrite($m, "", 5), fwrite($m, "xyz", null), fputs($m, 12, 1), ftell($m));
/// $r = fopen('php://memory', 'r');
/// var_dump(fwrite($r, "x"), fwrite($r, ""));
/// $p = __DIR__ . '/fwrite.txt'; $w = fopen($p, 'w'); fclose($w);
/// $f = fopen($p, 'r'); var_dump(fwrite($f, "x"));
/// try { fwrite("x", "y"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { fwrite($m, "x", "z"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(0)
/// int(2)
/// int(0)
/// int(3)
/// int(1)
/// int(6)
/// bool(false)
/// int(0)
///
/// Notice: fwrite(): Write of 1 bytes failed with errno=9 Bad file descriptor in %s on line 7
/// bool(false)
/// fwrite(): Argument #1 ($stream) must be of type resource, string given
/// fwrite(): Argument #3 ($length) must be of type ?int, string given
/// ```
fn fwrite(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 3)?;
    let stream = stream_argument(req, 1)?;
    let data = string_argument(req, 2)?;
    // `Z_PARAM_LONG_OR_NULL`: `null`, given or defaulted, is the whole.
    let maxlen = match req.argument(3) {
        Some(given) if !matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) => {
            let coerced = coerce_param(req, &given, &Type::Nullable(Box::new(Type::Int)), 3)?;
            coerced.as_int()
        }
        _ => None,
    };
    let bytes = data.as_bytes();
    let num_bytes = match maxlen {
        None => bytes.len(),
        Some(n) if n <= 0 => 0,
        Some(n) => usize::try_from(n).unwrap_or(usize::MAX).min(bytes.len()),
    };
    if num_bytes == 0 {
        return Ok(Value::int(0));
    }
    let wrote = stream.write(req, &bytes[..num_bytes]);
    Ok(wrote.map_or_else(
        || Value::bool(false),
        |n| Value::int(i64::try_from(n).unwrap_or(i64::MAX)),
    ))
}
