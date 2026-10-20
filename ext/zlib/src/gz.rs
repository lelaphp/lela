//! `zlib_fopen_wrapper.c` and `zlib.c`'s file natives: the
//! `compress.zlib://` wrapper, `php_stream_gzopen` and the `ZLIB` stream
//! kind over zlib's own `gzFile`, which reads and writes a duplicate of
//! the fd of the stream it was opened over; `gzopen`, `gzfile`
//! and `readgzfile`, which open without the wrapper; and the `gz*`
//! natives `zlib.stub.php` declares as `@alias` of `file.h`'s, served by
//! ext/standard's bodies under declarations of their own, as `ZEND_FALIAS`
//! serves them.

use std::os::fd::RawFd;

use php_ext::abi::{OrFalse, Path, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::to_long;
use php_ext::raising::{Level, Thrown, docref, warning};
use php_ext::request::{
    Cast, Opening, OwnedStream as _, Request, Stream, StreamCtx, StreamOps, StreamWrapper,
    Streams as _, Whence,
};
use php_ext::values::{Owner, PhpArray, PhpString, Resource};

/// `php_gz_stream_data_t`: the `gzFile` and the stream whose fd
/// it duplicated, whose owner the kind holds until it closes.
struct GzStream {
    file: Option<crate::ffi::GzFile>,
    inner: Option<Owner<Stream>>,
}

impl GzStream {
    /// `php_gziop_report_errors`: a system call's failure is php's
    /// notice with `errno` and its text.
    fn report(&mut self, req: &mut Request, count: usize, verb: &str) {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if self
            .file
            .as_mut()
            .is_some_and(crate::ffi::GzFile::failed_errno)
        {
            let text = format!(
                "{verb} of {count} bytes failed with errno={errno} {}",
                req.environment().os_error_text(errno)
            );
            docref(req, Level::Notice, &text);
        }
    }
}

impl StreamOps for GzStream {
    fn label(&self) -> &'static str {
        "ZLIB"
    }

    /// `php_gziop_write`: one `gzwrite`, capped at `INT_MAX`.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        let chunk = buf.len().min(i32::MAX as usize);
        let written = self.file.as_mut()?.write(&buf[..chunk]);
        if written < 0 {
            self.report(req, chunk, "Write");
            return None;
        }
        usize::try_from(written).ok()
    }

    /// `php_gziop_read`: one `gzread`, capped at `INT_MAX`, `eof` from
    /// `gzeof`.
    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        let chunk = buf.len().min(i32::MAX as usize);
        let file = self.file.as_mut()?;
        let read = file.read(&mut buf[..chunk]);
        if file.eof() {
            *eof = true;
        }
        if read < 0 {
            self.report(req, chunk, "Read");
            return None;
        }
        usize::try_from(read).ok()
    }

    /// `php_gziop_close`: the `gzFile` closed, then the stream under it
    /// with `php_stream_close`, which gives its resource up, so a closed
    /// `gzopen` stream leaves `get_resources()` as it found it.
    ///
    /// ```php
    /// stream_context_get_default();
    /// $b = get_resources();
    /// $f = gzopen(sys_get_temp_dir().'/gzclose-witness.gz', 'w'); gzclose($f); unset($f);
    /// foreach (array_diff_key(get_resources(), $b) as $k => $r) echo $k, "=", get_resource_type($r), "\n";
    /// $h = fopen('compress.zlib://'.sys_get_temp_dir().'/gzclose-witness.gz', 'r'); fclose($h); unset($h);
    /// gzfile(sys_get_temp_dir().'/gzclose-witness.gz'); readgzfile(sys_get_temp_dir().'/gzclose-witness.gz');
    /// var_dump(count(array_diff_key(get_resources(), $b)));
    /// unlink(sys_get_temp_dir().'/gzclose-witness.gz');
    /// echo "end\n";
    /// ```
    /// ```expect
    /// int(0)
    /// end
    /// ```
    fn close(&mut self, req: &mut Request) {
        self.file = None;
        if let Some(inner) = self.inner.take() {
            inner.free(req);
        }
    }

    /// `php_gziop_seek`: `SEEK_END` refused with php's warning, anything
    /// else `gzseek`.
    fn seek(
        &mut self,
        req: &mut Request,
        offset: i64,
        whence: Option<Whence>,
        position: &mut i64,
    ) -> Option<bool> {
        let whence = match whence {
            Some(Whence::Set) => libc_seek::SET,
            Some(Whence::Cur) => libc_seek::CUR,
            Some(Whence::End) => {
                docref(req, Level::Warning, "SEEK_END is not supported");
                return Some(false);
            }
            None => -1,
        };
        let sought = self.file.as_mut()?.seek(offset, whence);
        if sought < 0 {
            return Some(false);
        }
        *position = sought;
        Some(true)
    }

    fn seekable(&self) -> bool {
        true
    }

    /// `php_gziop_flush`: `gzflush(Z_SYNC_FLUSH)`.
    fn flush(&mut self, _req: &mut Request) -> bool {
        self.file.as_mut().is_some_and(crate::ffi::GzFile::flush)
    }

    /// `php_gziop_set_option`'s `PHP_STREAM_OPTION_LOCKING`, handed to
    /// the stream under the `gzFile`.
    fn flock(&mut self, req: &mut Request, operation: i64) -> Option<bool> {
        Some(self.inner.as_deref()?.flock(req, operation))
    }

    /// `php_gziop_set_option`'s `PHP_STREAM_OPTION_META_DATA_API`, handed
    /// to the stream under the `gzFile`: its `eof` is the plain file's,
    /// which the `gzFile` reads through a duplicate of its fd, so it
    /// stays `false` after the last byte.
    ///
    /// ```php
    /// $f = sys_get_temp_dir().'/gz-meta-witness.gz';
    /// file_put_contents($f, gzencode("abc"));
    /// $h = gzopen($f, 'r');
    /// var_dump(gzread($h, 10), gzeof($h));
    /// $m = stream_get_meta_data($h);
    /// var_dump($m['timed_out'], $m['blocked'], $m['eof']);
    /// gzclose($h);
    /// unlink($f);
    /// ```
    /// ```expect
    /// string(3) "abc"
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// ```
    fn meta_data_from(&self) -> Option<Stream> {
        self.inner.as_deref().copied()
    }

    /// `PHP_STREAM_FLAG_NO_BUFFER`, which `php_stream_gzopen` sets.
    fn buffered(&self) -> bool {
        false
    }
}

/// `SEEK_SET` and `SEEK_CUR` as `gzseek` reads them.
mod libc_seek {
    pub(super) const SET: i32 = 0;
    pub(super) const CUR: i32 = 1;
}

/// `php_stream_gzopen`: refused for a `mode` with `+`; the path after
/// `compress.zlib://` or `zlib:` opened through its own wrapper under the
/// same options, cast to its fd and handed to `gzdopen` on a
/// duplicate, with the `zlib` `level` of its `php_stream_context` set on it; a stream of
/// the `ZLIB` kind, unbuffered. Its own warnings are made under
/// `REPORT_ERRORS` alone, which the wrapper is never handed. The
/// wrapper's opener, `gzip_stream_wops.stream_opener`. The inner open is
/// handed the caller's `opened_path` with the options, so an include
/// through the wrapper is named as the file it read, its real path; and
/// `STREAM_OPEN_FOR_INCLUDE` among them, so `allow_url_include` holds a
/// URL wrapper inside it to the include.
///
/// ```php
/// $d = sys_get_temp_dir() . '/gz-include-' . getmypid();
/// mkdir($d);
/// file_put_contents("$d/a.php", gzencode('<?php return __FILE__;'));
/// $r = realpath($d);
/// echo str_replace($r, 'R', include "compress.zlib://$d/./a.php"), "\n";
/// echo str_replace($r, 'R', get_included_files()[1]), "\n";
/// unlink("$d/a.php");
/// rmdir($d);
/// var_dump(@include 'compress.zlib://data:text/plain,<?php return 7;');
/// ```
/// ```expect
/// R/a.php
/// R/a.php
/// bool(false)
/// ```
fn open(
    req: &mut Request,
    path: &[u8],
    mode: &[u8],
    opening: Opening,
) -> Result<Option<Owner<Stream>>, Thrown> {
    if mode.contains(&b'+') {
        if !opening.warn {
            return Ok(None);
        }
        docref(
            req,
            Level::Warning,
            "Cannot open a zlib stream for reading and writing at the same time!",
        );
        return Ok(None);
    }
    let path = if path.len() >= 16 && path[..16].eq_ignore_ascii_case(b"compress.zlib://") {
        &path[16..]
    } else if path.len() >= 5 && path[..5].eq_ignore_ascii_case(b"zlib:") {
        &path[5..]
    } else {
        path
    };
    let opening = Opening {
        must_seek: true,
        will_cast: true,
        ..opening
    };
    let Some(inner) = req.open_stream_with(path, mode, opening)? else {
        return Ok(None);
    };
    let Some(fd) = inner.cast(req, Cast::Fd, true) else {
        inner.free(req);
        return Ok(None);
    };
    let file = match gzdopen(req, fd, mode) {
        Ok(file) => file,
        Err(thrown) => {
            inner.free(req);
            return Err(thrown);
        }
    };
    let Some(file) = file else {
        if opening.warn {
            docref(req, Level::Warning, "gzopen failed");
        }
        inner.free(req);
        return Ok(None);
    };
    let kind = GzStream {
        file: Some(file),
        inner: Some(inner),
    };
    req.new_stream(Box::new(kind), mode).map(Some)
}

/// `gzdopen(dup(fd), mode)`, and `gzsetparams` with the `level` of the
/// `php_stream_context` the open was handed, php's plain warning where zlib refuses
/// it.
fn gzdopen(
    req: &mut Request,
    fd: RawFd,
    mode: &[u8],
) -> Result<Option<crate::ffi::GzFile>, Thrown> {
    let Some(mut file) = crate::ffi::GzFile::dopen(fd, mode) else {
        return Ok(None);
    };
    if let Some(context) = req.stream_ctx()
        && let Some(level) = context.option(req, b"zlib", b"level")
    {
        let level = to_long(req, &level)?;
        if !file.set_level(level) {
            warning(req, "failed setting compression level");
        }
    }
    Ok(Some(file))
}

/// `php_stream_gzip_wrapper`, `compress.zlib`, labelled `ZLIB`: an
/// opener and nothing else, so a stat, a directory, an unlink or a
/// rename through it is refused as php refuses them.
///
/// ```php
/// $f = 'zlib-wrapper-witness.gz';
/// var_dump(file_put_contents("compress.zlib://$f", "line one\nline two\n"));
/// var_dump(bin2hex(substr(file_get_contents($f), 0, 3)), file_get_contents("compress.zlib://$f"));
/// $h = fopen("compress.zlib://$f", 'r');
/// $m = stream_get_meta_data($h);
/// var_dump($m['wrapper_type'], $m['stream_type'], $m['seekable'], fgets($h), ftell($h), fseek($h, 0, SEEK_END));
/// fclose($h);
/// var_dump(fopen("compress.zlib://$f", 'r+'), @file_exists("compress.zlib://$f"), @unlink("compress.zlib://$f"));
/// unlink($f);
/// ```
/// ```expect
/// int(18)
/// string(6) "1f8b08"
/// string(18) "line one
/// line two
/// "
///
/// Warning: fseek(): SEEK_END is not supported in %s on line 7
/// string(4) "ZLIB"
/// string(4) "ZLIB"
/// bool(true)
/// string(9) "line one
/// "
/// int(9)
/// int(-1)
///
/// Warning: fopen(compress.zlib://zlib-wrapper-witness.gz): Failed to open stream: operation failed in %s on line 9
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
pub(crate) fn wrapper() -> StreamWrapper {
    StreamWrapper {
        scheme: "compress.zlib".into(),
        label: "ZLIB",
        is_url: false,
        open: Some(open),
        open_dir: None,
        stat: None,
        unlink: None,
        rename: None,
        mkdir: None,
        rmdir: None,
        set_stat: None,
        stream_stat: None,
    }
}

/// What the three natives below open with, `php_stream_gzopen` under
/// `REPORT_ERRORS` and `USE_PATH` and through no wrapper, with
/// `FG(default_context)` as its `php_stream_context`.
fn open_plain(
    req: &mut Request,
    filename: &[u8],
    mode: &[u8],
    use_include_path: bool,
) -> Result<Option<Owner<Stream>>, Thrown> {
    let context = Some(StreamCtx::default_ctx(req)?);
    let opening = Opening {
        use_path: use_include_path,
        warn: true,
        ..Opening::default()
    };
    req.with_stream_ctx(context, |req| open(req, filename, mode, opening))
}

/// `gzopen(string $filename, string $mode, bool $use_include_path =
/// false)`: the stream, as a resource of the `stream` type.
///
/// ```php
/// $f = 'gzopen-witness.gz';
/// $h = gzopen($f, 'wb9');
/// var_dump(gzwrite($h, str_repeat('abc', 10)), gzputs($h, "\nend"), gztell($h), gzclose($h));
/// $h = gzopen($f, 'r');
/// var_dump(stream_get_meta_data($h));
/// var_dump(gzread($h, 5), gzgetc($h), gzgets($h), gzeof($h), gztell($h), gzseek($h, 2), gztell($h), gzrewind($h), gzread($h, 3));
/// var_dump(flock($h, LOCK_SH), fflush($h), fstat($h), gzpassthru($h));
/// echo "\n";
/// var_dump(gzeof($h), gzclose($h), gzopen($f, 'r+'), gzopen('/no/such/dir/x.gz', 'r'));
/// unlink($f);
/// ```
/// ```expect
/// int(30)
/// int(4)
/// int(34)
/// bool(true)
/// array(7) {
///   ["timed_out"]=>
///   bool(false)
///   ["blocked"]=>
///   bool(true)
///   ["eof"]=>
///   bool(false)
///   ["stream_type"]=>
///   string(4) "ZLIB"
///   ["mode"]=>
///   string(1) "r"
///   ["unread_bytes"]=>
///   int(0)
///   ["seekable"]=>
///   bool(true)
/// }
/// string(5) "abcab"
/// string(1) "c"
/// string(25) "abcabcabcabcabcabcabcabc
/// "
/// bool(false)
/// int(31)
/// int(0)
/// int(2)
/// bool(true)
/// string(3) "abc"
/// abcabcabcabcabcabcabcabcabc
/// endbool(true)
/// bool(false)
/// bool(false)
/// int(31)
///
///
/// Warning: gzopen(): Cannot open a zlib stream for reading and writing at the same time! in %s on line 10
///
/// Warning: gzopen(/no/such/dir/x.gz): Failed to open stream: No such file or directory in %s on line 10
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn gzopen(
    req: &mut Request,
    filename: Path,
    mode: PhpString,
    #[php(default = false)] use_include_path: bool,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    let Path(filename) = filename;
    let opened = open_plain(req, filename.as_bytes(), mode.as_bytes(), use_include_path)?;
    Ok(Untyped(opened.map_or(OrFalse::False, |stream| {
        OrFalse::Value(stream.into_resource())
    })))
}

/// `gzfile(string $filename, bool $use_include_path = false):
/// array|false`: each line as `php_stream_gets` reads it into php's 8 KiB
/// buffer, cut at a NUL as `add_index_string` copies it.
///
/// ```php
/// $f = 'gzfile-witness.gz';
/// file_put_contents($f, gzencode("one\ntwo\n\nthree"));
/// var_dump(gzfile($f), readgzfile($f), gzfile('/no/such/file.gz'));
/// file_put_contents($f, "not gzip\nat all");
/// var_dump(gzfile($f));
/// unlink($f);
/// ```
/// ```expect
/// one
/// two
///
/// three
/// Warning: gzfile(/no/such/file.gz): Failed to open stream: No such file or directory in %s on line 4
/// array(4) {
///   [0]=>
///   string(4) "one
/// "
///   [1]=>
///   string(4) "two
/// "
///   [2]=>
///   string(1) "
/// "
///   [3]=>
///   string(5) "three"
/// }
/// int(14)
/// bool(false)
/// array(2) {
///   [0]=>
///   string(9) "not gzip
/// "
///   [1]=>
///   string(6) "at all"
/// }
/// ```
#[php_function]
fn gzfile(
    req: &mut Request,
    filename: Path,
    #[php(default = false)] use_include_path: bool,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Path(filename) = filename;
    let Some(stream) = open_plain(req, filename.as_bytes(), b"rb", use_include_path)? else {
        return Ok(OrFalse::False);
    };
    let lines = read_lines(req, *stream);
    stream.free(req);
    Ok(OrFalse::Value(lines?))
}

/// `gzfile`'s loop: each line of `stream` into a new array.
fn read_lines(req: &mut Request, stream: Stream) -> Result<Owner<PhpArray>, Thrown> {
    let mut lines = PhpArray::new(req)?;
    loop {
        let line = match stream.get_line(req, Some(8191)) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(thrown) => {
                lines.give_up(req);
                return Err(thrown);
            }
        };
        let cut = PhpString::new(req, php_ext::coerce::c_str(&line))?;
        lines.append(req, cut.into_value())?;
    }
    Ok(lines)
}

/// `readgzfile(string $filename, bool $use_include_path = false):
/// int|false`: the file uncompressed onto the output, and its length.
/// ```php
/// $f = 'readgzfile-witness.gz';
/// file_put_contents($f, gzencode("one\ntwo\n"));
/// var_dump(readgzfile($f), readgzfile('/no/such/file.gz'));
/// unlink($f);
/// ```
/// ```expect
/// one
/// two
///
/// Warning: readgzfile(/no/such/file.gz): Failed to open stream: No such file or directory in %s on line 4
/// int(8)
/// bool(false)
/// ```
#[php_function]
fn readgzfile(
    req: &mut Request,
    filename: Path,
    #[php(default = false)] use_include_path: bool,
) -> Result<OrFalse<i64>, Thrown> {
    let Path(filename) = filename;
    let Some(stream) = open_plain(req, filename.as_bytes(), b"rb", use_include_path)? else {
        return Ok(OrFalse::False);
    };
    let size = php_ext_standard::fpassthru(req, *stream);
    stream.free(req);
    Ok(OrFalse::Value(size?))
}

/// `gzwrite($stream, string $data, ?int $length = null): int|false`,
/// `ZEND_FALIAS(gzwrite, fwrite, ...)`, and `gzputs`, its alias.
///
/// ```php
/// try { gzwrite('x', 'y'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $h = fopen('php://memory', 'r+');
/// var_dump(gzputs($h, 'abcdef', 3), gzrewind($h), gzread($h, 10), gzeof($h), gzclose($h));
/// ```
/// ```expect
/// gzwrite(): Argument #1 ($stream) must be of type resource, string given
/// int(3)
/// bool(true)
/// string(3) "abc"
/// bool(true)
/// bool(true)
/// ```
#[php_function(aliases(gzputs))]
fn gzwrite(
    req: &mut Request,
    stream: Stream,
    data: PhpString,
    #[php(default = null)] length: Option<i64>,
) -> OrFalse<i64> {
    php_ext_standard::fwrite(req, stream, data, length)
}

/// `gzrewind($stream): bool`, `ZEND_FALIAS(gzrewind, rewind, ...)`.
///
/// ```php
/// $h = fopen('php://memory', 'r+'); fwrite($h, 'abc');
/// var_dump(gzrewind($h), ftell($h));
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// ```
#[php_function]
fn gzrewind(req: &mut Request, stream: Stream) -> bool {
    php_ext_standard::rewind(req, stream)
}

/// `gzclose($stream): bool`, `ZEND_FALIAS(gzclose, fclose, ...)`.
///
/// ```php
/// $h = fopen('php://memory', 'r');
/// var_dump(gzclose($h));
/// try { gzclose($h); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// gzclose(): Argument #1 ($stream) must be an open stream resource
/// ```
#[php_function]
fn gzclose(req: &mut Request, stream: Stream) -> bool {
    php_ext_standard::fclose(req, stream)
}

/// `gzeof($stream): bool`, `ZEND_FALIAS(gzeof, feof, ...)`.
///
/// ```php
/// $h = fopen('php://memory', 'r+'); fwrite($h, 'a'); rewind($h);
/// var_dump(gzeof($h), fread($h, 2), gzeof($h));
/// ```
/// ```expect
/// bool(false)
/// string(1) "a"
/// bool(true)
/// ```
#[php_function]
fn gzeof(req: &mut Request, stream: Stream) -> bool {
    php_ext_standard::feof(req, stream)
}

/// `gzgetc($stream): string|false`, `ZEND_FALIAS(gzgetc, fgetc, ...)`.
///
/// ```php
/// $h = fopen('php://memory', 'r+'); fwrite($h, 'ab'); rewind($h);
/// var_dump(gzgetc($h), gzgetc($h), gzgetc($h));
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "b"
/// bool(false)
/// ```
#[php_function]
fn gzgetc(req: &mut Request, stream: Stream) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    php_ext_standard::fgetc(req, stream)
}

/// `gzpassthru($stream): int`, `ZEND_FALIAS(gzpassthru, fpassthru, ...)`.
///
/// ```php
/// $h = fopen('php://memory', 'r+'); fwrite($h, "abc\n"); rewind($h);
/// var_dump(gzpassthru($h));
/// ```
/// ```expect
/// abc
/// int(4)
/// ```
#[php_function]
fn gzpassthru(req: &mut Request, stream: Stream) -> Result<i64, Thrown> {
    php_ext_standard::fpassthru(req, stream)
}

/// `gzseek($stream, int $offset, int $whence = SEEK_SET): int`,
/// `ZEND_FALIAS(gzseek, fseek, ...)`.
///
/// ```php
/// $f = 'gzseek-witness.gz';
/// $h = gzopen($f, 'w');
/// gzwrite($h, 'abc');
/// var_dump(gzseek($h, 5), gztell($h), gzseek($h, 1, SEEK_CUR), gzseek($h, 1), gzseek($h, 0, SEEK_END));
/// gzwrite($h, 'z');
/// gzclose($h);
/// var_dump(bin2hex(file_get_contents("compress.zlib://$f")));
/// unlink($f);
/// ```
/// ```expect
/// Warning: gzseek(): SEEK_END is not supported in %s on line 5
/// int(0)
/// int(5)
/// int(0)
/// int(-1)
/// int(-1)
/// string(8) "6162637a"
/// ```
#[php_function]
fn gzseek(
    req: &mut Request,
    stream: Stream,
    offset: i64,
    #[php(default = SEEK_SET)] whence: i64,
) -> i64 {
    php_ext_standard::fseek(req, stream, offset, whence)
}

/// `gztell($stream): int|false`, `ZEND_FALIAS(gztell, ftell, ...)`.
///
/// ```php
/// $h = fopen('php://memory', 'r+'); fwrite($h, 'abcd');
/// var_dump(gztell($h));
/// ```
/// ```expect
/// int(4)
/// ```
#[php_function]
fn gztell(req: &mut Request, stream: Stream) -> OrFalse<i64> {
    php_ext_standard::ftell(req, stream)
}

/// `gzread($stream, int $length): string|false`,
/// `ZEND_FALIAS(gzread, fread, ...)`.
///
/// ```php
/// $h = fopen('php://memory', 'r+'); fwrite($h, 'abcd'); rewind($h);
/// var_dump(gzread($h, 3));
/// try { gzread($h, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "abc"
/// gzread(): Argument #2 ($length) must be greater than 0
/// ```
#[php_function]
fn gzread(
    req: &mut Request,
    stream: Stream,
    length: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    php_ext_standard::fread(req, stream, length)
}

/// `gzgets($stream, ?int $length = null): string|false`,
/// `ZEND_FALIAS(gzgets, fgets, ...)`.
///
/// ```php
/// $h = fopen('php://memory', 'r+'); fwrite($h, "ab\ncd"); rewind($h);
/// var_dump(gzgets($h), gzgets($h, 2), gzgets($h), gzgets($h));
/// ```
/// ```expect
/// string(3) "ab
/// "
/// string(1) "c"
/// string(1) "d"
/// bool(false)
/// ```
#[php_function]
fn gzgets(
    req: &mut Request,
    stream: Stream,
    #[php(default = null)] length: Option<i64>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    php_ext_standard::fgets(req, stream, length)
}
