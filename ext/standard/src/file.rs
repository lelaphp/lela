//! `file.c`'s natives (`Z/ext/standard/file.c`) over the engine's
//! streams facet (design §4.3 seam 5), which is where
//! `php_stream_open_wrapper_ex`, `php_stream_read` and their siblings
//! live, beside the directory natives of `dir.c` and
//! `stream_get_meta_data` of `streamsfuncs.c`, the two that read the same
//! facet. In the stub's order (`basic_functions.stub.php`): `opendir`,
//! `closedir`, `rewinddir` and `readdir`; `rewind`, `fclose`, `feof`,
//! `fgetc`, `fgets`, `fread`, `fopen`, `ftruncate`, `fstat`, `fseek`,
//! `ftell`, `fflush`, `fwrite` and its alias `fputs`,
//! `file_get_contents` and `file_put_contents`; then
//! `stream_get_meta_data`. A `$stream` parameter is
//! `PHP_Z_PARAM_STREAM`: a resource, and one that is an open stream.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::call::lookup_class;
use php_ext::coerce::{coerce_param, to_string, value_name};
use php_ext::decl::{ConstDecl, ConstExpr, FunctionDecl, Name, Parameter, Type};
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref, throw};
use php_ext::request::{Request, Stream, Whence};
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::output::Entry;
use crate::{
    State, argument_count_between, bool_argument, constant, declared, long_argument, parameter,
    string_argument,
};

/// `SEEK_SET`, `SEEK_CUR` and `SEEK_END` as `<stdio.h>` has them.
const SEEK_SET: i64 = 0;
const SEEK_CUR: i64 = 1;
const SEEK_END: i64 = 2;

/// `PHP_LOCK_SH`, `PHP_LOCK_EX`, `PHP_LOCK_UN` and `PHP_LOCK_NB`
/// (`Z/ext/standard/flock_compat.h:46`), what `LOCK_*` are to a program.
const PHP_LOCK_SH: i64 = 1;
const PHP_LOCK_EX: i64 = 2;
const PHP_LOCK_UN: i64 = 3;
const PHP_LOCK_NB: i64 = 4;

/// `flock(2)`'s `LOCK_EX`, what `file_put_contents` hands
/// `php_stream_lock` and tests its `$flags` for.
const LOCK_EX: i64 = 2;

/// `PHP_FILE_*` (`Z/ext/standard/file.h`), the bits of `$flags`.
const PHP_FILE_USE_INCLUDE_PATH: i64 = 1;
const PHP_FILE_IGNORE_NEW_LINES: i64 = 2;
const PHP_FILE_SKIP_EMPTY_LINES: i64 = 4;
const PHP_FILE_APPEND: i64 = 8;
const PHP_FILE_NO_DEFAULT_CONTEXT: i64 = 16;

/// The constants `file.stub.php` declares that the natives here read,
/// in its order, less the two 8.1 deprecated.
///
/// ```php
/// var_dump(SEEK_SET, SEEK_CUR, SEEK_END, LOCK_SH, LOCK_EX, LOCK_UN, LOCK_NB, FILE_USE_INCLUDE_PATH, FILE_IGNORE_NEW_LINES, FILE_SKIP_EMPTY_LINES, FILE_APPEND, FILE_NO_DEFAULT_CONTEXT);
/// ```
/// ```expect
/// int(0)
/// int(1)
/// int(2)
/// int(1)
/// int(2)
/// int(3)
/// int(4)
/// int(1)
/// int(2)
/// int(4)
/// int(8)
/// int(16)
/// ```
pub(crate) fn constants() -> Vec<ConstDecl> {
    vec![
        constant(b"SEEK_SET", SEEK_SET),
        constant(b"SEEK_CUR", SEEK_CUR),
        constant(b"SEEK_END", SEEK_END),
        constant(b"LOCK_SH", PHP_LOCK_SH),
        constant(b"LOCK_EX", PHP_LOCK_EX),
        constant(b"LOCK_UN", PHP_LOCK_UN),
        constant(b"LOCK_NB", PHP_LOCK_NB),
        constant(b"FILE_USE_INCLUDE_PATH", PHP_FILE_USE_INCLUDE_PATH),
        constant(b"FILE_IGNORE_NEW_LINES", PHP_FILE_IGNORE_NEW_LINES),
        constant(b"FILE_SKIP_EMPTY_LINES", PHP_FILE_SKIP_EMPTY_LINES),
        constant(b"FILE_APPEND", PHP_FILE_APPEND),
        constant(b"FILE_NO_DEFAULT_CONTEXT", PHP_FILE_NO_DEFAULT_CONTEXT),
    ]
}

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
    let fwrite_decl = |name: &[u8]| {
        declared(
            name,
            &[
                stream_parameter(),
                parameter(b"data", Type::String),
                Parameter {
                    default: Some(ConstExpr::Null),
                    ..parameter(b"length", Type::Nullable(Box::new(Type::Int)))
                },
            ],
            Type::Union(vec![Type::Int, Type::False]),
        )
    };
    let mut natives = dir_natives();
    natives.extend(read_natives());
    natives.extend(vec![
        (
            declared(
                b"ftruncate",
                &[stream_parameter(), parameter(b"size", Type::Int)],
                Type::Bool,
            ),
            ftruncate as Entry,
        ),
        (
            declared(
                b"fstat",
                &[stream_parameter()],
                Type::Union(vec![Type::Array, Type::False]),
            ),
            fstat,
        ),
        (
            declared(
                b"fseek",
                &[
                    stream_parameter(),
                    parameter(b"offset", Type::Int),
                    Parameter {
                        default: Some(ConstExpr::Constant(Name(b"SEEK_SET".as_slice().into()))),
                        ..parameter(b"whence", Type::Int)
                    },
                ],
                Type::Int,
            ),
            fseek,
        ),
        (
            declared(
                b"ftell",
                &[stream_parameter()],
                Type::Union(vec![Type::Int, Type::False]),
            ),
            ftell,
        ),
        (
            declared(b"fflush", &[stream_parameter()], Type::Bool),
            fflush,
        ),
        (fwrite_decl(b"fwrite"), fwrite),
        (fwrite_decl(b"fputs"), fwrite),
    ]);
    natives.extend(contents_natives());
    natives
}

/// The untyped `$stream` the stub declares.
fn stream_parameter() -> Parameter {
    Parameter {
        ty: None,
        ..parameter(b"stream", Type::Mixed)
    }
}

/// An untyped parameter defaulting to `null`, the stub's `$context` and
/// `$dir_handle`.
fn untyped_null(name: &[u8]) -> Parameter {
    Parameter {
        ty: None,
        default: Some(ConstExpr::Null),
        ..parameter(name, Type::Mixed)
    }
}

/// `string|false`.
fn string_or_false() -> Type {
    Type::Union(vec![Type::String, Type::False])
}

/// `file.c`'s from `rewind` to `fopen`, in the stub's order.
fn read_natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            declared(b"rewind", &[stream_parameter()], Type::Bool),
            rewind as Entry,
        ),
        (
            declared(b"fclose", &[stream_parameter()], Type::Bool),
            fclose,
        ),
        (declared(b"feof", &[stream_parameter()], Type::Bool), feof),
        (
            declared(b"fgetc", &[stream_parameter()], string_or_false()),
            fgetc,
        ),
        (
            declared(
                b"fgets",
                &[
                    stream_parameter(),
                    Parameter {
                        default: Some(ConstExpr::Null),
                        ..parameter(b"length", Type::Nullable(Box::new(Type::Int)))
                    },
                ],
                string_or_false(),
            ),
            fgets,
        ),
        (
            declared(
                b"fread",
                &[stream_parameter(), parameter(b"length", Type::Int)],
                string_or_false(),
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
                        untyped_null(b"context"),
                    ],
                    Type::Mixed,
                )
            },
            fopen,
        ),
    ]
}

/// `dir.c`'s four over a directory stream, in the stub's order.
fn dir_natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            FunctionDecl {
                return_type: None,
                ..declared(
                    b"opendir",
                    &[
                        parameter(b"directory", Type::String),
                        untyped_null(b"context"),
                    ],
                    Type::Mixed,
                )
            },
            opendir as Entry,
        ),
        (
            declared(b"closedir", &[untyped_null(b"dir_handle")], Type::Void),
            closedir,
        ),
        (
            declared(b"rewinddir", &[untyped_null(b"dir_handle")], Type::Void),
            rewinddir,
        ),
        (
            declared(
                b"readdir",
                &[untyped_null(b"dir_handle")],
                string_or_false(),
            ),
            readdir,
        ),
    ]
}

/// `file_get_contents` and `file_put_contents`, the last of `file.c`'s
/// here, and `stream_get_meta_data`.
fn contents_natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            declared(
                b"file_get_contents",
                &[
                    parameter(b"filename", Type::String),
                    Parameter {
                        default: Some(ConstExpr::Bool(false)),
                        ..parameter(b"use_include_path", Type::Bool)
                    },
                    untyped_null(b"context"),
                    Parameter {
                        default: Some(ConstExpr::Long(0)),
                        ..parameter(b"offset", Type::Int)
                    },
                    Parameter {
                        default: Some(ConstExpr::Null),
                        ..parameter(b"length", Type::Nullable(Box::new(Type::Int)))
                    },
                ],
                string_or_false(),
            ),
            file_get_contents as Entry,
        ),
        (
            declared(
                b"file_put_contents",
                &[
                    parameter(b"filename", Type::String),
                    parameter(b"data", Type::Mixed),
                    Parameter {
                        default: Some(ConstExpr::Long(0)),
                        ..parameter(b"flags", Type::Int)
                    },
                    untyped_null(b"context"),
                ],
                Type::Union(vec![Type::Int, Type::False]),
            ),
            file_put_contents,
        ),
        (
            declared(b"stream_get_meta_data", &[stream_parameter()], Type::Array),
            stream_get_meta_data,
        ),
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

/// `PHP_Z_PARAM_STREAM_OR_NULL` then
/// `php_dir_get_directory_stream_from_user_arg`
/// (`Z/ext/standard/dir.c:165`): the directory stream the argument at
/// position one names; no argument or `null` is the 8.4 deprecation and
/// then the directory `opendir` opened last, or php's `No resource
/// supplied` where there is none; a stream that is no directory is php's
/// `must be a valid Directory resource`.
fn dir_argument(req: &mut Request) -> Result<Stream, Thrown> {
    let given = req.argument(1).unwrap_or_default().deref();
    let stream = match given.as_ref() {
        ValueRef::Null | ValueRef::Undef => None,
        ValueRef::Resource(_) => Some(
            Stream::from_value(req, &given)
                .ok_or_else(|| arg_type_error(req, 1, "be an open stream resource"))?,
        ),
        _ => {
            let name = value_name(req, &given);
            return Err(arg_type_error(
                req,
                1,
                &format!("be of type resource or null, {name} given"),
            ));
        }
    };
    let stream = if let Some(stream) = stream {
        stream
    } else {
        docref(
            req,
            Level::Deprecated,
            "Passing null is deprecated, instead the last opened directory stream should be provided",
        );
        let default = req.state::<State>().default_dir;
        let Some(stream) = default.and_then(|res| Stream::from_value(req, &res.as_value())) else {
            return Err(throw(req, "TypeError", "No resource supplied"));
        };
        stream
    };
    if !stream.is_dir(req) {
        return Err(arg_type_error(req, 1, "be a valid Directory resource"));
    }
    Ok(stream)
}

/// `php_set_default_dir` (`Z/ext/standard/dir.c:62`): the owner the last
/// directory held given up, and one taken on the new one, or none.
fn set_default_dir(req: &mut Request, stream: Option<Stream>) {
    let res = stream.map(Stream::resource);
    if let Some(res) = res {
        req.addref(&res.as_value());
    }
    if let Some(old) = std::mem::replace(&mut req.state::<State>().default_dir, res) {
        req.release(old.as_value());
    }
}

/// `opendir(string $directory, $context = null)`, `PHP_FUNCTION(opendir)`
/// (`Z/ext/standard/dir.c:146`): `php_stream_opendir` under
/// `REPORT_ERRORS`, the stream a resource of the `stream` type that
/// becomes the directory `readdir()` reads with no argument, or `false`
/// after the warning. The plain files wrapper gives `.` and `..` first
/// and then the entries in the order `readdir(3)` has them.
///
/// ```php
/// $d = __DIR__; fclose(fopen("$d/b.txt", 'w')); fclose(fopen("$d/a.txt", 'w'));
/// $h = opendir($d); var_dump($h);
/// $names = []; while (($name = readdir($h)) !== false) { $names[] = $name; }
/// sort($names); var_dump($names, readdir($h));
/// var_dump(opendir("$d/missing"), opendir("$d/a.txt"), opendir('php://memory'), opendir(''));
/// ```
/// ```expect
/// resource(%d) of type (stream)
/// array(5) {
///   [0]=>
///   string(1) "."
///   [1]=>
///   string(2) ".."
///   [2]=>
///   string(5) "a.txt"
///   [3]=>
///   string(5) "b.txt"
///   [4]=>
///   string(11) "witness.php"
/// }
/// bool(false)
///
/// Warning: opendir(%s/missing): Failed to open directory: No such file or directory in %s on line 6
///
/// Warning: opendir(%s/a.txt): Failed to open directory: Not a directory in %s on line 6
///
/// Warning: opendir(php://memory): Failed to open directory: not implemented in %s on line 6
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn opendir(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let directory = path_argument(req, 1)?;
    context_argument(req, 2)?;
    let Some(dir) = req.open_dir(directory.as_bytes())? else {
        return Ok(Value::bool(false));
    };
    set_default_dir(req, Some(dir));
    Ok(dir.as_value())
}

/// `closedir($dir_handle = null): void`, `PHP_FUNCTION(closedir)`
/// (`Z/ext/standard/dir.c:205`): the directory's resource closed, and
/// the last opened one forgotten where it was that. A stream that is no
/// directory is refused, and `fclose` refuses a directory.
///
/// ```php
/// $h = opendir(__DIR__);
/// var_dump(closedir($h), $h, fclose($f = opendir(__DIR__)), readdir($f));
/// try { closedir(fopen('php://memory', 'r')); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { closedir("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { closedir($h); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// closedir(); try { closedir(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Warning: fclose(): cannot close the provided stream, as it must not be manually closed in %s on line 3
/// NULL
/// resource(%d) of type (Unknown)
/// bool(false)
/// string(1) "."
/// closedir(): Argument #1 ($dir_handle) must be a valid Directory resource
/// closedir(): Argument #1 ($dir_handle) must be of type resource or null, string given
/// closedir(): Argument #1 ($dir_handle) must be an open stream resource
///
/// Deprecated: closedir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 7
///
/// Deprecated: closedir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 7
/// No resource supplied
/// ```
fn closedir(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 1)?;
    let dir = dir_argument(req)?;
    let res = dir.resource();
    dir.close(req);
    let is_default = req
        .state::<State>()
        .default_dir
        .is_some_and(|default| default.id() == res.id());
    if is_default {
        set_default_dir(req, None);
    }
    Ok(Value::null())
}

/// `rewinddir($dir_handle = null): void`, `PHP_FUNCTION(rewinddir)`
/// (`Z/ext/standard/dir.c:245`): `php_stream_rewinddir`, a seek to the
/// start, which the plain files wrapper takes as `rewinddir(3)`.
///
/// ```php
/// $h = opendir(__DIR__);
/// var_dump(readdir($h), readdir($h), rewinddir($h), readdir($h) === readdir(opendir(__DIR__)));
/// ```
/// ```expect
/// string(1) "."
/// string(2) ".."
/// NULL
/// bool(true)
/// ```
fn rewinddir(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 1)?;
    let dir = dir_argument(req)?;
    dir.seek(req, 0, Whence::Set);
    Ok(Value::null())
}

/// `readdir($dir_handle = null): string|false`, `PHP_FUNCTION(readdir)`
/// (`Z/ext/standard/dir.c:276`): the next entry's name, `false` past the
/// last; with no argument, the directory `opendir` opened last.
///
/// ```php
/// $d = __DIR__; fclose(fopen("$d/only.txt", 'w'));
/// $h = opendir($d); $names = []; while (($name = readdir()) !== false) { $names[] = $name; }
/// sort($names); var_dump($names);
/// try { readdir(fopen('php://memory', 'r')); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Deprecated: readdir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 3
///
/// Deprecated: readdir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 3
///
/// Deprecated: readdir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 3
///
/// Deprecated: readdir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 3
///
/// Deprecated: readdir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 3
/// array(4) {
///   [0]=>
///   string(1) "."
///   [1]=>
///   string(2) ".."
///   [2]=>
///   string(8) "only.txt"
///   [3]=>
///   string(11) "witness.php"
/// }
/// readdir(): Argument #1 ($dir_handle) must be a valid Directory resource
/// ```
fn readdir(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 1)?;
    let dir = dir_argument(req)?;
    Ok(dir
        .read_dir(req)?
        .map_or_else(|| Value::bool(false), Value::string))
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
    if stream.is_dir(req) {
        docref(
            req,
            Level::Warning,
            "cannot close the provided stream, as it must not be manually closed",
        );
        return Ok(Value::bool(false));
    }
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

/// `fgetc($stream): string|false`, `PHP_FUNCTION(fgetc)`
/// (`Z/ext/standard/file.c:910`): `php_stream_getc`, one byte read, and
/// `false` where none was.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "ab"); rewind($m);
/// var_dump(fgetc($m), fgetc($m), feof($m), fgetc($m), feof($m), ftell($m));
/// $w = fopen(__DIR__ . '/fgetc.txt', 'w'); var_dump(fgetc($w));
/// try { fgetc("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "b"
/// bool(false)
/// bool(false)
/// bool(true)
/// int(2)
///
/// Notice: fgetc(): Read of 8192 bytes failed with errno=9 Bad file descriptor in %s on line 4
/// bool(false)
/// fgetc(): Argument #1 ($stream) must be of type resource, string given
/// ```
fn fgetc(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let stream = stream_argument(req, 1)?;
    Ok(match stream.read(req, 1)? {
        Some(byte) if !byte.is_empty() => Value::string(byte),
        Some(empty) => {
            req.release(Value::string(empty));
            Value::bool(false)
        }
        None => Value::bool(false),
    })
}

/// `fgets($stream, ?int $length = null): string|false`,
/// `PHP_FUNCTION(fgets)` (`Z/ext/standard/file.c:862`):
/// `php_stream_get_line`, the next line with its `\n`, of any length for
/// `null` and at most `$length - 1` bytes otherwise, a length at or
/// below zero being php's `ValueError`; `false` where no byte was left.
/// The line is read through the read buffer a chunk at a time, so a
/// `php://memory` read by line has its bytes in the buffer, which a
/// seek or a write drops.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "one\ntwo\nthree"); rewind($m);
/// var_dump(fgets($m), ftell($m), fgets($m, 3), fgets($m, 2), fgets($m), fgets($m), fgets($m), feof($m));
/// rewind($m); var_dump(fgets($m, 1), fread($m, 3), fgets($m, null), fwrite($m, "!"), rewind($m), fgets($m));
/// try { fgets($m, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "one
/// "
/// int(4)
/// string(2) "tw"
/// string(1) "o"
/// string(1) "
/// "
/// string(5) "three"
/// bool(false)
/// bool(true)
/// bool(false)
/// string(3) "one"
/// string(1) "
/// "
/// int(1)
/// bool(true)
/// string(4) "one
/// "
/// fgets(): Argument #2 ($length) must be greater than 0
/// ```
///
/// A plain file's line longer than a chunk is read whole, and the
/// length is asked for as a string before a byte is read, so one the
/// memory limit cannot hold is php's fatal.
///
/// ```php
/// $p = __DIR__ . '/fgets.txt'; $w = fopen($p, 'w'); fwrite($w, str_repeat("x", 10000) . "\nend"); fclose($w);
/// $r = fopen($p, 'r'); var_dump(strlen(fgets($r)), fgets($r), fgets($r), feof($r));
/// fgets($r, PHP_INT_MAX);
/// ```
/// ```expect exit=255
/// int(10001)
/// string(3) "end"
/// bool(false)
/// bool(true)
///
/// Fatal error: Allowed memory size of 134217728 bytes exhausted (tried to allocate %d bytes) in %s on line 4
/// ```
fn fgets(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let stream = stream_argument(req, 1)?;
    let maxlen = match req.argument(2) {
        Some(given) if !matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) => {
            let coerced = coerce_param(req, &given, &Type::Nullable(Box::new(Type::Int)), 2)?;
            coerced.as_int()
        }
        _ => None,
    };
    let maxlen = match maxlen {
        Some(len) if len <= 0 => return Err(arg_value_error(req, 2, "be greater than 0")),
        Some(len) => Some(usize::try_from(len).unwrap_or(usize::MAX)),
        None => None,
    };
    Ok(stream
        .get_line(req, maxlen)?
        .map_or_else(|| Value::bool(false), Value::string))
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

/// `ftruncate($stream, int $size): bool`, `PHP_FUNCTION(ftruncate)`
/// (`Z/ext/standard/file.c:1299`): a size below zero is php's
/// `ValueError`, a stream that does not truncate is php's warning and
/// `false`, and otherwise whether `php_stream_truncate_set_size` took
/// the size. Neither the position nor the kind's offset moves.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "hello");
/// var_dump(ftruncate($m, 2), ftell($m), rewind($m), fread($m, 10), ftruncate($m, 4), rewind($m), fread($m, 10) === "he\0\0");
/// var_dump(ftruncate(fopen('php://memory', 'r'), 0), ftruncate(fopen('php://output', 'w'), 0));
/// $p = __DIR__ . '/ftruncate.txt'; $w = fopen($p, 'w+'); fwrite($w, "0123456789");
/// var_dump(ftruncate($w, 3), ftell($w), fstat($w)['size'], rewind($w), fread($w, 10), ftruncate(fopen($p, 'r'), 0));
/// try { ftruncate($m, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(5)
/// bool(true)
/// string(2) "he"
/// bool(true)
/// bool(true)
/// bool(true)
///
/// Warning: ftruncate(): Can't truncate this stream! in %s on line 4
/// bool(false)
/// bool(false)
/// bool(true)
/// int(10)
/// int(3)
/// bool(true)
/// string(3) "012"
/// bool(false)
/// ftruncate(): Argument #2 ($size) must be greater than or equal to 0
/// ```
fn ftruncate(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let stream = stream_argument(req, 1)?;
    let size = long_argument(req, 2)?;
    if size < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    let Some(taken) = stream.truncate(req, size) else {
        docref(req, Level::Warning, "Can't truncate this stream!");
        return Ok(Value::bool(false));
    };
    Ok(Value::bool(taken))
}

/// The names `php_fstat` gives the thirteen fields after their numbers
/// (`Z/ext/standard/file.c:1327`).
const STAT_NAMES: [&[u8]; 13] = [
    b"dev", b"ino", b"mode", b"nlink", b"uid", b"gid", b"rdev", b"size", b"atime", b"mtime",
    b"ctime", b"blksize", b"blocks",
];

/// `fstat($stream): array|false`, `PHP_FUNCTION(fstat)`
/// (`Z/ext/standard/file.c:1395`) over `php_fstat`: the thirteen fields
/// of `php_stream_stat` under the numbers `0` to `12` and then under
/// their names, or `false` for a stream with no stat. A `php://memory`
/// is a regular file of its length, `0666` or `0444` read only, on
/// device `0xC`.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "abc"); var_dump(fstat($m));
/// $p = __DIR__ . '/fstat.txt'; $w = fopen($p, 'w'); fwrite($w, "hello"); $s = fstat($w);
/// var_dump(count($s), $s['size'], $s[7], $s['mode'] & 0170000, $s['nlink'], $s['mtime'] > 0, $s['blksize'] > 0);
/// var_dump(fstat(fopen('php://output', 'w')), fstat(opendir(__DIR__)));
/// ```
/// ```expect
/// array(26) {
///   [0]=>
///   int(12)
///   [1]=>
///   int(0)
///   [2]=>
///   int(33206)
///   [3]=>
///   int(1)
///   [4]=>
///   int(0)
///   [5]=>
///   int(0)
///   [6]=>
///   int(-1)
///   [7]=>
///   int(3)
///   [8]=>
///   int(0)
///   [9]=>
///   int(0)
///   [10]=>
///   int(0)
///   [11]=>
///   int(-1)
///   [12]=>
///   int(-1)
///   ["dev"]=>
///   int(12)
///   ["ino"]=>
///   int(0)
///   ["mode"]=>
///   int(33206)
///   ["nlink"]=>
///   int(1)
///   ["uid"]=>
///   int(0)
///   ["gid"]=>
///   int(0)
///   ["rdev"]=>
///   int(-1)
///   ["size"]=>
///   int(3)
///   ["atime"]=>
///   int(0)
///   ["mtime"]=>
///   int(0)
///   ["ctime"]=>
///   int(0)
///   ["blksize"]=>
///   int(-1)
///   ["blocks"]=>
///   int(-1)
/// }
/// int(26)
/// int(5)
/// int(5)
/// int(32768)
/// int(1)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
fn fstat(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let stream = stream_argument(req, 1)?;
    let Some(sb) = stream.stat(req) else {
        return Ok(Value::bool(false));
    };
    let mut out = PhpArray::new(req)?;
    for field in sb {
        out.append(req, Value::int(field))?;
    }
    for (name, field) in STAT_NAMES.iter().zip(sb) {
        let key = Key::Str(PhpString::new(req, name)?);
        out.set(req, key, Value::int(field))?;
    }
    Ok(Value::array(out))
}

/// `fseek($stream, int $offset, int $whence = SEEK_SET): int`,
/// `PHP_FUNCTION(fseek)` (`Z/ext/standard/file.c:1061`):
/// `php_stream_seek`, `0` where it was taken and `-1` where not. A
/// `$whence` that is none of `SEEK_SET`, `SEEK_CUR` and `SEEK_END` is
/// handed on as it is, which a stream that seeks answers `-1` to and one
/// that cannot is php's warning for; a seek before the start is `-1` and
/// one past the end is taken.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "hello");
/// var_dump(fseek($m, 1), ftell($m), fread($m, 2), fseek($m, -1, SEEK_CUR), fread($m, 10), fseek($m, -2, SEEK_END), fread($m, 10));
/// var_dump(fseek($m, 10, SEEK_END), ftell($m), fseek($m, -1), ftell($m), fseek($m, -20, SEEK_END), ftell($m), fseek($m, 0, 7), ftell($m));
/// $o = fopen('php://output', 'w'); var_dump(fseek($o, 0), fseek($o, 1, SEEK_CUR), fseek($o, 0, 7));
/// ```
/// ```expect
/// int(0)
/// int(1)
/// string(2) "el"
/// int(0)
/// string(3) "llo"
/// int(0)
/// string(2) "lo"
/// int(0)
/// int(15)
/// int(-1)
/// int(15)
/// int(-1)
/// bool(false)
/// int(-1)
/// int(0)
///
/// Warning: fseek(): Stream does not support seeking in %s on line 5
///
/// Warning: fseek(): Stream does not support seeking in %s on line 5
/// int(-1)
/// int(-1)
/// int(-1)
/// ```
///
/// On a plain file a seek that lands inside what the read buffer holds
/// moves the cursor in it; any other is `lseek(2)` and the buffer
/// dropped.
///
/// ```php
/// $p = __DIR__ . '/fseek.txt'; $w = fopen($p, 'w'); fwrite($w, "0123456789"); fclose($w);
/// $r = fopen($p, 'r'); var_dump(fread($r, 2), fseek($r, 5), fread($r, 2), fseek($r, 2, SEEK_CUR), fread($r, 10), feof($r));
/// var_dump(fseek($r, 20), ftell($r), fread($r, 1), feof($r), fseek($r, 0, 9), ftell($r), fseek($r, 1, SEEK_SET), fgetc($r));
/// ```
/// ```expect
/// string(2) "01"
/// int(0)
/// string(2) "56"
/// int(0)
/// string(1) "9"
/// bool(true)
/// int(0)
/// int(20)
/// string(0) ""
/// bool(true)
/// int(-1)
/// int(20)
/// int(0)
/// string(1) "1"
/// ```
fn fseek(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 3)?;
    let stream = stream_argument(req, 1)?;
    let offset = long_argument(req, 2)?;
    let whence = if req.argument_count() >= 3 {
        long_argument(req, 3)?
    } else {
        SEEK_SET
    };
    let sought = stream.seek(req, offset, Whence::from_int(whence));
    Ok(Value::int(if sought { 0 } else { -1 }))
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

/// `fflush($stream): bool`, `PHP_FUNCTION(fflush)`
/// (`Z/ext/standard/file.c:1009`): `php_stream_flush`, the flush owed at
/// close taken now, and whether the kind's flush answered `0`, which
/// every kind here does.
///
/// ```php
/// $p = __DIR__ . '/fflush.txt'; $w = fopen($p, 'w');
/// var_dump(fwrite($w, "ab"), fflush($w), fflush(fopen('php://memory', 'r')), fflush(fopen('php://temp', 'r')), fflush(fopen('php://output', 'w')));
/// try { fflush("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// fflush(): Argument #1 ($stream) must be of type resource, string given
/// ```
fn fflush(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let stream = stream_argument(req, 1)?;
    Ok(Value::bool(stream.flush(req)))
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
///
/// A `php://temp` keeps its bytes in the memory stream it encloses
/// until a write would reach its `maxmemory`, `PHP_STREAM_MAX_MEM` by
/// default, and then spills them to a temporary file, a third resource,
/// which `fstat` reads from then on and the close unlinks.
///
/// ```php
/// $t = fopen('php://temp/maxmemory:4', 'w+'); $a = fopen('php://memory', 'r');
/// var_dump(fwrite($t, "ab"), fstat($t)['mode'], fwrite($t, "cdefgh"), fstat($t)['mode'], fstat($t)['size']);
/// $b = fopen('php://memory', 'r');
/// var_dump((int)$a - (int)$t, (int)$b - (int)$a, rewind($t), fread($t, 100), ftell($t), stream_get_meta_data($t)['stream_type']);
/// $big = fopen('php://temp', 'w+'); fwrite($big, str_repeat("x", 2 * 1024 * 1024 - 1)); $c = fopen('php://memory', 'r');
/// fwrite($big, "y"); $e = fopen('php://memory', 'r');
/// var_dump((int)$c - (int)$big, (int)$e - (int)$c, fstat($big)['size'], fstat($big)['mode']);
/// ```
/// ```expect
/// int(2)
/// int(33206)
/// int(6)
/// int(33152)
/// int(8)
/// int(2)
/// int(2)
/// bool(true)
/// string(8) "abcdefgh"
/// int(8)
/// string(4) "TEMP"
/// int(2)
/// int(2)
/// int(2097152)
/// int(33152)
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

/// `Z_PARAM_RESOURCE_OR_NULL` for a `$context`: a resource or `null`,
/// taken and not read, since no wrapper here reads a `$context`; anything
/// else is the parser's `TypeError`.
fn context_argument(req: &mut Request, position: u32) -> Result<(), Thrown> {
    let given = req.argument(position).unwrap_or_default().deref();
    match given.as_ref() {
        ValueRef::Null | ValueRef::Undef | ValueRef::Resource(_) => Ok(()),
        _ => {
            let name = value_name(req, &given);
            Err(arg_type_error(
                req,
                position,
                &format!("be of type resource or null, {name} given"),
            ))
        }
    }
}

/// `Z_PARAM_PATH`: the string argument at `position`, with php's
/// `ValueError` for a null byte in it.
fn path_argument(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let path = string_argument(req, position)?;
    if path.as_bytes().contains(&0) {
        return Err(arg_value_error(req, position, "not contain any null bytes"));
    }
    Ok(path)
}

/// `file_get_contents(string $filename, bool $use_include_path = false, $context = null, int $offset = 0, ?int $length = null): string|false`,
/// `PHP_FUNCTION(file_get_contents)` (`Z/ext/standard/file.c:377`): a
/// length below zero is php's `ValueError`; the file opened `rb` under
/// `REPORT_ERRORS`, with `USE_PATH` where asked, and `false` after the
/// warning where it was not; a plain file read unbuffered; a nonzero
/// offset sought first, from the start where it is positive and from the
/// end where it is negative, a seek refused being php's warning and
/// `false`; then `_php_stream_copy_to_mem`, all of it or `$length`
/// bytes, and the empty string where nothing was read. `$context` is
/// taken and not read: no wrapper here reads one.
///
/// ```php
/// $p = __DIR__ . '/fgc.txt'; $w = fopen($p, 'w'); fwrite($w, "from stream"); fclose($w);
/// var_dump(file_get_contents($p), file_get_contents($p, false, null, 5), file_get_contents($p, false, null, -3), file_get_contents($p, false, null, 2, 3), file_get_contents($p, false, null, 100), file_get_contents($p, false, null, 0, 0));
/// var_dump(file_get_contents($p, false, null, -100), file_get_contents(__DIR__ . '/missing.txt'));
/// try { file_get_contents($p, false, null, 0, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { file_get_contents(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(11) "from stream"
/// string(6) "stream"
/// string(3) "eam"
/// string(3) "om "
/// string(0) ""
/// string(0) ""
///
/// Warning: file_get_contents(): Failed to seek to position -100 in the stream in %s on line 4
///
/// Warning: file_get_contents(%s/missing.txt): Failed to open stream: No such file or directory in %s on line 4
/// bool(false)
/// bool(false)
/// file_get_contents(): Argument #5 ($length) must be greater than or equal to 0
/// Path must not be empty
/// ```
///
/// A directory opens as a file does and is read unbuffered, so the one
/// read that fails asks for what `fstat` says the directory holds and a
/// chunk more.
///
/// ```php
/// $d = __DIR__; var_dump(file_get_contents($d) === "", file_get_contents($d, false, null, 0, 10));
/// ```
/// ```expect
///
/// Notice: file_get_contents(): Read of 8288 bytes failed with errno=21 Is a directory in %s on line 2
///
/// Notice: file_get_contents(): Read of 10 bytes failed with errno=21 Is a directory in %s on line 2
/// bool(true)
/// string(0) ""
/// ```
fn file_get_contents(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 5)?;
    let filename = path_argument(req, 1)?;
    let use_include_path = if req.argument_count() >= 2 {
        bool_argument(req, 2)?
    } else {
        false
    };
    context_argument(req, 3)?;
    let offset = if req.argument_count() >= 4 {
        long_argument(req, 4)?
    } else {
        0
    };
    let maxlen = match req.argument(5) {
        Some(given) if !matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) => {
            let coerced = coerce_param(req, &given, &Type::Nullable(Box::new(Type::Int)), 5)?;
            coerced.as_int()
        }
        _ => None,
    };
    let maxlen = match maxlen {
        Some(len) if len < 0 => {
            return Err(arg_value_error(req, 5, "be greater than or equal to 0"));
        }
        Some(len) => Some(usize::try_from(len).unwrap_or(usize::MAX)),
        None => None,
    };
    let Some(stream) = req.open_stream(filename.as_bytes(), b"rb", use_include_path)? else {
        return Ok(Value::bool(false));
    };
    if stream.is_stdio(req) {
        stream.set_read_buffer(req, false);
    }
    if offset != 0 {
        let whence = if offset > 0 { Whence::Set } else { Whence::End };
        if !stream.seek(req, offset, whence) {
            docref(
                req,
                Level::Warning,
                &format!("Failed to seek to position {offset} in the stream"),
            );
            stream.close(req);
            return Ok(Value::bool(false));
        }
    }
    let contents = stream.copy_to_mem(req, maxlen)?;
    stream.close(req);
    Ok(Value::string(contents))
}

/// `file_put_contents(string $filename, mixed $data, int $flags = 0, $context = null): int|false`,
/// `PHP_FUNCTION(file_put_contents)` (`Z/ext/standard/file.c:439`): the
/// file opened `wb`, or `ab` under `FILE_APPEND`, or `cb` under
/// `LOCK_EX`, which only a path with no scheme or `file://` may ask for
/// and which truncates after `flock(2)` took the file; then `$data` written: a
/// stream copied whole, a scalar or `null` as its string, an array's
/// elements one after the other as strings, an object through its
/// `__toString`. What was written is the count, and `false` is an object
/// that has no `__toString`, a write that took less, or a file that did
/// not open.
///
/// ```php
/// $p = __DIR__ . '/fpc.txt';
/// var_dump(file_put_contents($p, "one"), file_get_contents($p), file_put_contents($p, 12, FILE_APPEND), file_get_contents($p));
/// var_dump(file_put_contents($p, [1, 2.5, "x", null, true]), file_get_contents($p), file_put_contents($p, "lock", LOCK_EX), file_get_contents($p));
/// class S { function __toString(): string { return "str"; } }
/// var_dump(file_put_contents($p, new S), file_get_contents($p), file_put_contents($p, new stdClass), file_get_contents($p), file_put_contents($p, ""));
/// $m = fopen('php://memory', 'w+'); fwrite($m, "from stream"); rewind($m);
/// var_dump(file_put_contents($p, $m), file_get_contents($p), file_put_contents('php://memory', "x"));
/// var_dump(file_put_contents('php://memory', "x", LOCK_EX), file_put_contents(__DIR__, "x"));
/// try { file_put_contents($p, "x", 0, 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// string(3) "one"
/// int(2)
/// string(5) "one12"
/// int(6)
/// string(6) "12.5x1"
/// int(4)
/// string(4) "lock"
/// int(3)
/// string(3) "str"
/// bool(false)
/// string(0) ""
/// int(0)
/// int(11)
/// string(11) "from stream"
/// int(1)
///
/// Warning: file_put_contents(): Exclusive locks may only be set for regular files in %s on line 9
///
/// Warning: file_put_contents(%s): Failed to open stream: Is a directory in %s on line 9
/// bool(false)
/// bool(false)
/// file_put_contents(): Argument #4 ($context) must be of type resource or null, int given
/// ```
///
/// An array element that is itself an array is php's warning and the
/// word `Array`.
///
/// ```php
/// $p = __DIR__ . '/fpc.txt'; var_dump(file_put_contents($p, ["a", ["b"]]), file_get_contents($p));
/// ```
/// ```expect
///
/// Warning: Array to string conversion in %s on line 2
/// int(6)
/// string(6) "aArray"
/// ```
fn file_put_contents(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 4)?;
    let filename = path_argument(req, 1)?;
    let data = req.argument(2).unwrap_or_default().deref();
    let flags = if req.argument_count() >= 3 {
        long_argument(req, 3)?
    } else {
        0
    };
    context_argument(req, 4)?;
    let source = match data.as_ref() {
        ValueRef::Resource(_) => match Stream::from_value(req, &data) {
            Some(stream) => Some(stream),
            None => {
                return Err(throw(
                    req,
                    "TypeError",
                    "file_put_contents(): supplied resource is not a valid stream resource",
                ));
            }
        },
        _ => None,
    };
    let name = filename.as_bytes();
    let mode: &[u8] = if flags & PHP_FILE_APPEND != 0 {
        b"ab"
    } else if flags & LOCK_EX != 0 {
        let has_scheme = name.windows(3).any(|w| w == b"://");
        if has_scheme && !(name.len() >= 7 && name[..7].eq_ignore_ascii_case(b"file://")) {
            docref(
                req,
                Level::Warning,
                "Exclusive locks may only be set for regular files",
            );
            return Ok(Value::bool(false));
        }
        b"cb"
    } else {
        b"wb"
    };
    let use_include_path = flags & PHP_FILE_USE_INCLUDE_PATH != 0;
    let Some(stream) = req.open_stream(name, mode, use_include_path)? else {
        return Ok(Value::bool(false));
    };
    if flags & LOCK_EX != 0 && !stream.flock(req, LOCK_EX) {
        stream.close(req);
        docref(
            req,
            Level::Warning,
            "Exclusive locks are not supported for this stream",
        );
        return Ok(Value::bool(false));
    }
    if mode[0] == b'c' {
        stream.truncate(req, 0);
    }
    let written = write_data(req, stream, source, &data, name);
    stream.close(req);
    Ok(match written? {
        Some(n) => Value::int(i64::try_from(n).unwrap_or(i64::MAX)),
        None => Value::bool(false),
    })
}

/// `file_put_contents`'s switch over `$data`: how many bytes went to
/// `stream`, `None` for php's `-1`.
fn write_data(
    req: &mut Request,
    stream: Stream,
    source: Option<Stream>,
    data: &Value,
    filename: &[u8],
) -> Result<Option<usize>, Thrown> {
    let whole = |req: &mut Request, bytes: &[u8]| -> Option<usize> {
        if bytes.is_empty() {
            return Some(0);
        }
        let wrote = stream.write(req, bytes)?;
        if wrote != bytes.len() {
            docref(
                req,
                Level::Warning,
                &format!(
                    "Only {wrote} of {} bytes written, possibly out of free disk space",
                    bytes.len()
                ),
            );
            return None;
        }
        Some(wrote)
    };
    match data.as_ref() {
        ValueRef::Resource(_) => {
            let Some(source) = source else {
                return Ok(None);
            };
            let bytes = source.copy_to_mem(req, None)?;
            let wrote = if bytes.is_empty() {
                Some(0)
            } else {
                stream.write(req, bytes.as_bytes())
            };
            req.release(Value::string(bytes));
            Ok(wrote)
        }
        ValueRef::Null
        | ValueRef::Bool(_)
        | ValueRef::Int(_)
        | ValueRef::Float(_)
        | ValueRef::Str(_) => {
            let text = to_string(req, data)?;
            let wrote = whole(req, text.as_bytes());
            req.release(Value::string(text));
            Ok(wrote)
        }
        ValueRef::Array(array) => {
            let mut numbytes = 0;
            for (_, element) in array.snapshot() {
                let text = to_string(req, &element)?;
                let len = text.len();
                if len > 0 {
                    numbytes += len;
                    let wrote = stream.write(req, text.as_bytes());
                    if wrote != Some(len) {
                        let name = String::from_utf8_lossy(filename).into_owned();
                        docref(
                            req,
                            Level::Warning,
                            &format!("Failed to write {len} bytes to {name}"),
                        );
                        req.release(Value::string(text));
                        return Ok(None);
                    }
                }
                req.release(Value::string(text));
            }
            Ok(Some(numbytes))
        }
        ValueRef::Object(object) => {
            if !has_to_string(req, *object)? {
                return Ok(None);
            }
            let text = to_string(req, data)?;
            let wrote = whole(req, text.as_bytes());
            req.release(Value::string(text));
            Ok(wrote)
        }
        ValueRef::Undef | ValueRef::Reference(_) => Ok(None),
    }
}

/// `ce->__tostring`, which `zend_std_cast_object_tostring` reads before
/// it calls anything: a `__toString` the class or a class it extends,
/// or one of their traits, declares, or `Stringable` among what it
/// implements.
fn has_to_string(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    if let Some(stringable) = lookup_class(req, b"Stringable")?
        && object.instance_of(req, stringable)
    {
        return Ok(true);
    }
    let mut at = Some(object.class());
    for _ in 0..64 {
        let Some(class) = at else {
            break;
        };
        let Some(decl) = req.class_decl(class) else {
            break;
        };
        let declares = |methods: &[php_ext::decl::MethodDecl]| {
            methods
                .iter()
                .any(|m| m.function.name.0.eq_ignore_ascii_case(b"__toString"))
        };
        if declares(&decl.methods) {
            return Ok(true);
        }
        let parent = decl.parent.clone();
        let traits = decl.traits.clone();
        for name in traits {
            if let Some(used) = lookup_class(req, &name.0)?
                && req.class_decl(used).is_some_and(|t| declares(&t.methods))
            {
                return Ok(true);
            }
        }
        at = match parent {
            Some(name) => lookup_class(req, &name.0)?,
            None => None,
        };
    }
    Ok(false)
}

/// `stream_get_meta_data($stream): array`,
/// `PHP_FUNCTION(stream_get_meta_data)`
/// (`Z/ext/standard/streamsfuncs.c:514`): what the stream's kind reports
/// of itself, then `wrapper_type`, `stream_type`, `mode`, `unread_bytes`,
/// `seekable` and `uri`. A `php://temp` reports nothing of itself, and a
/// plain file's `eof` is what its last read left, not `feof`'s answer.
///
/// ```php
/// var_dump(stream_get_meta_data(fopen('php://temp', 'r+')), stream_get_meta_data(fopen('php://memory', 'a')));
/// $p = __DIR__ . '/meta.txt'; $w = fopen($p, 'w'); fwrite($w, "a\nb\n"); fclose($w);
/// $r = fopen($p, 'r'); fgets($r); var_dump(stream_get_meta_data($r));
/// var_dump(stream_get_meta_data(opendir(__DIR__)), stream_get_meta_data(fopen('php://output', 'w')));
/// ```
/// ```expect
/// array(6) {
///   ["wrapper_type"]=>
///   string(3) "PHP"
///   ["stream_type"]=>
///   string(4) "TEMP"
///   ["mode"]=>
///   string(3) "w+b"
///   ["unread_bytes"]=>
///   int(0)
///   ["seekable"]=>
///   bool(true)
///   ["uri"]=>
///   string(10) "php://temp"
/// }
/// array(9) {
///   ["timed_out"]=>
///   bool(false)
///   ["blocked"]=>
///   bool(true)
///   ["eof"]=>
///   bool(false)
///   ["wrapper_type"]=>
///   string(3) "PHP"
///   ["stream_type"]=>
///   string(6) "MEMORY"
///   ["mode"]=>
///   string(3) "a+b"
///   ["unread_bytes"]=>
///   int(0)
///   ["seekable"]=>
///   bool(true)
///   ["uri"]=>
///   string(12) "php://memory"
/// }
/// array(9) {
///   ["timed_out"]=>
///   bool(false)
///   ["blocked"]=>
///   bool(true)
///   ["eof"]=>
///   bool(false)
///   ["wrapper_type"]=>
///   string(9) "plainfile"
///   ["stream_type"]=>
///   string(5) "STDIO"
///   ["mode"]=>
///   string(1) "r"
///   ["unread_bytes"]=>
///   int(2)
///   ["seekable"]=>
///   bool(true)
///   ["uri"]=>
///   string(%d) "%s/meta.txt"
/// }
/// array(8) {
///   ["timed_out"]=>
///   bool(false)
///   ["blocked"]=>
///   bool(true)
///   ["eof"]=>
///   bool(false)
///   ["wrapper_type"]=>
///   string(9) "plainfile"
///   ["stream_type"]=>
///   string(3) "dir"
///   ["mode"]=>
///   string(1) "r"
///   ["unread_bytes"]=>
///   int(0)
///   ["seekable"]=>
///   bool(true)
/// }
/// array(9) {
///   ["timed_out"]=>
///   bool(false)
///   ["blocked"]=>
///   bool(true)
///   ["eof"]=>
///   bool(false)
///   ["wrapper_type"]=>
///   string(3) "PHP"
///   ["stream_type"]=>
///   string(6) "Output"
///   ["mode"]=>
///   string(2) "wb"
///   ["unread_bytes"]=>
///   int(0)
///   ["seekable"]=>
///   bool(false)
///   ["uri"]=>
///   string(12) "php://output"
/// }
/// ```
fn stream_get_meta_data(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let stream = stream_argument(req, 1)?;
    Ok(Value::array(stream.meta_data(req)?))
}
