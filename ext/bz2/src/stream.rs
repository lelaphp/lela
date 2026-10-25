//! `bz2.c`'s stream half: the `compress.bzip2://` wrapper,
//! `_php_stream_bz2open` and the `BZip2` stream kind, `bzopen`, `bzread`,
//! the three natives `bz2.stub.php` declares as implementation aliases of
//! `file.h`'s, and `bzerrno`, `bzerrstr` and `bzerror`.
//!
//! php's kind holds libbz2's `BZFILE`, which reads and writes a C `FILE`:
//! the path opened directly, `BZ2_bzopen`, and where that fails the path
//! opened as a php stream and its fd handed to `BZ2_bzdopen`. The kind
//! here holds that file layer ported, `bzlib.c`'s `BZ2_bzRead`,
//! `BZ2_bzWrite` and `BZ2_bzWriteClose64` with the `BZ_MAX_UNUSED` buffer
//! they fill and drain, over the file opened directly or over the php
//! stream itself. Its last error, `bzf->lastErr`, lives in the request's
//! state under the file's number, where `bzerror` reads it.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};

use php_ext::abi::{OrFalse, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::value_name;
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref, type_error};
use php_ext::request::{
    Cast, HostPath, Opening, OwnedStream as _, Request, Stream, StreamOps, StreamWrapper,
    Streams as _,
};
use php_ext::values::{Owner, PhpArray, PhpString, Resource, Value, ValueRef};

use crate::State;
use crate::ffi::{Action, Code, Compress, Decompress};

/// `BZ_MAX_UNUSED`, the size of the buffer `bzFile` reads the file into
/// and writes compressed bytes out of.
const MAX_UNUSED: usize = 5000;

/// The number a `BZip2` stream's file is known by in [`State`], while it
/// is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FileId(u64);

/// `bzf->handle`: the file libbz2 reads and writes, with whether a read
/// of it met its end (`feof`): the one `BZ2_bzopen` opened on the path, or
/// the copy of a php stream's fd 8.5.12's `php_bz2_bzdopen` hands
/// `BZ2_bzdopen`, since libbz2 closes the fd it is given. Beside the copy,
/// the owner of that stream the kind holds, php's `stream_res` with its
/// `GC_ADDREF`: the stream may be closed under it, and the copy reads
/// and writes on all the same.
struct Handle {
    file: File,
    eof: bool,
    inner: Option<Owner<Stream>>,
}

impl Handle {
    /// The file opened on the path.
    fn file(file: File) -> Handle {
        Handle {
            file,
            eof: false,
            inner: None,
        }
    }

    /// `php_bz2_bzdopen` over `inner`, cast to its fd already: the fd
    /// copied, `dup`, and the copy the file; `None` where it cannot be,
    /// the owner given back to the caller to free.
    fn over(req: &mut Request, inner: Owner<Stream>) -> Result<Handle, Owner<Stream>> {
        match inner.dup_fd(req) {
            Some(fd) => Ok(Handle {
                file: File::from(fd),
                eof: false,
                inner: Some(inner),
            }),
            None => Err(inner),
        }
    }

    /// `fread(buf, 1, BZ_MAX_UNUSED, handle)`: up to `MAX_UNUSED` bytes,
    /// `None` for `ferror`.
    fn fill(&mut self, buf: &mut Vec<u8>) -> Option<()> {
        buf.clear();
        buf.resize(MAX_UNUSED, 0);
        let mut got = 0;
        while got < MAX_UNUSED {
            match self.file.read(&mut buf[got..]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => got += n,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
        buf.truncate(got);
        Some(())
    }

    /// `myfeof(handle)`: whether the file has no byte left to read.
    fn at_end(&self) -> bool {
        self.eof
    }

    /// `fwrite` of the whole of `bytes`, `n != n2` its failure.
    fn put(&mut self, bytes: &[u8]) -> bool {
        self.file.write_all(bytes).is_ok()
    }

    /// `fclose(handle)`, then the stream under it where there is one:
    /// `php_stream_free` where it is still open, and its owner given up
    /// where the program closed it already, `zend_list_delete`.
    fn close(self, req: &mut Request) {
        drop(self.file);
        if let Some(inner) = self.inner {
            inner.free(req);
        }
    }
}

/// `bzf->writing` and the stream `bzFile` carries for it: a
/// decompressing one with the bytes read from the file and not yet taken,
/// or a compressing one.
enum Direction {
    Reading {
        stream: Decompress,
        buf: Vec<u8>,
        taken: usize,
    },
    Writing {
        stream: Compress,
    },
}

/// `php_bz2_stream_data_t` and the `bzFile` it holds: the file, whether
/// a read or a write of it failed (`ferror(bzf->handle)`), and the
/// direction; both given up at the close.
struct BzFile {
    id: FileId,
    handle: Option<Handle>,
    failed: bool,
    direction: Option<Direction>,
}

/// The block size, the direction and `s` of a `mode` string, as
/// `bzopen_or_bzdopen` reads it: the last of `r` and `w` wins, a digit
/// sets the block size, `s` the small decompressor.
struct Mode {
    writing: bool,
    small: bool,
    block_size: i32,
}

impl Mode {
    fn parse(mode: &[u8]) -> Mode {
        let mut parsed = Mode {
            writing: false,
            small: false,
            block_size: 9,
        };
        for &c in mode {
            match c {
                b'r' => parsed.writing = false,
                b'w' => parsed.writing = true,
                b's' => parsed.small = true,
                b'0'..=b'9' => parsed.block_size = i32::from(c - b'0'),
                _ => {}
            }
        }
        parsed
    }

    /// `fopen(path, mode2)`, `mode2` being `wb` or `rb`: the path the
    /// guard let through, created or truncated for writing.
    fn open(&self, path: &HostPath) -> Option<File> {
        let mut options = OpenOptions::new();
        if self.writing {
            options.write(true).create(true).truncate(true);
        } else {
            options.read(true);
        }
        options.open(path).ok()
    }

    /// `BZ2_bzWriteOpen` at the block size clamped to 1..9 and a work
    /// factor of 30, or `BZ2_bzReadOpen` with `s`.
    fn direction(&self) -> Option<Direction> {
        if self.writing {
            let stream = Compress::new(self.block_size.clamp(1, 9), 30).ok()?;
            Some(Direction::Writing { stream })
        } else {
            let stream = Decompress::new(self.small).ok()?;
            Some(Direction::Reading {
                stream,
                buf: Vec::new(),
                taken: 0,
            })
        }
    }
}

impl BzFile {
    /// `BZ_SETERR`: the file's last error.
    fn set(&self, req: &mut Request, code: Code) {
        req.state::<State>().errors.insert(self.id, code);
    }

    fn last(&self, req: &mut Request) -> Code {
        req.state::<State>()
            .errors
            .get(&self.id)
            .copied()
            .unwrap_or(Code::Ok)
    }

    /// `BZ2_bzread`: nothing once the stream has ended, else
    /// `BZ2_bzRead`'s count, `None` for its error.
    fn bzread(&mut self, req: &mut Request, out: &mut [u8]) -> Option<usize> {
        if self.last(req) == Code::StreamEnd {
            return Some(0);
        }
        let (count, code) = self.read_into(out);
        self.set(req, code);
        matches!(code, Code::Ok | Code::StreamEnd).then_some(count)
    }

    /// `BZ2_bzRead`: decompress into `out` until it is full or the stream
    /// ends, reading the file `MAX_UNUSED` bytes at a time; an error
    /// answers no bytes, and a file that ends inside the stream
    /// `BZ_UNEXPECTED_EOF`.
    fn read_into(&mut self, out: &mut [u8]) -> (usize, Code) {
        let (Some(handle), Some(direction)) = (self.handle.as_mut(), self.direction.as_mut())
        else {
            return (0, Code::SequenceError);
        };
        let Direction::Reading { stream, buf, taken } = direction else {
            return (0, Code::SequenceError);
        };
        if out.is_empty() {
            return (0, Code::Ok);
        }
        let mut written = 0;
        loop {
            if self.failed {
                return (0, Code::IoError);
            }
            if *taken == buf.len() && !handle.at_end() {
                if handle.fill(buf).is_none() {
                    self.failed = true;
                    return (0, Code::IoError);
                }
                *taken = 0;
            }
            let step = stream.decompress(&buf[*taken..], &mut out[written..]);
            *taken += step.read;
            written += step.written;
            match step.code {
                Code::StreamEnd => return (written, Code::StreamEnd),
                Code::Ok => {}
                failed => return (0, failed),
            }
            if handle.at_end() && *taken == buf.len() && written < out.len() {
                return (0, Code::UnexpectedEof);
            }
            if written == out.len() {
                return (written, Code::Ok);
            }
        }
    }

    /// `BZ2_bzwrite`: `BZ2_bzWrite`, the whole for its success.
    fn bzwrite(&mut self, req: &mut Request, data: &[u8]) -> Option<usize> {
        let code = self.write_from(data);
        self.set(req, code);
        (code == Code::Ok).then_some(data.len())
    }

    /// `BZ2_bzWrite`: compress `data` with `BZ_RUN`, writing each
    /// `MAX_UNUSED` bytes of what comes out to the file.
    fn write_from(&mut self, data: &[u8]) -> Code {
        let (Some(handle), Some(Direction::Writing { stream })) =
            (self.handle.as_mut(), self.direction.as_mut())
        else {
            return Code::SequenceError;
        };
        if self.failed {
            return Code::IoError;
        }
        if data.is_empty() {
            return Code::Ok;
        }
        let mut out = [0; MAX_UNUSED];
        let mut read = 0;
        loop {
            let step = stream.compress(&data[read..], &mut out, Action::Run);
            read += step.read;
            if step.code != Code::RunOk {
                return step.code;
            }
            if step.written > 0 && !handle.put(&out[..step.written]) {
                self.failed = true;
                return Code::IoError;
            }
            if read == data.len() {
                return Code::Ok;
            }
        }
    }

    /// `BZ2_bzWriteClose64` not abandoned: `BZ_FINISH` until the stream
    /// ends, each piece written to the file, where the last write left no
    /// error.
    fn finish(&mut self, req: &mut Request) {
        if self.failed || self.last(req) != Code::Ok {
            return;
        }
        let (Some(handle), Some(Direction::Writing { stream })) =
            (self.handle.as_mut(), self.direction.as_mut())
        else {
            return;
        };
        let mut out = [0; MAX_UNUSED];
        loop {
            let step = stream.compress(&[], &mut out, Action::Finish);
            if !matches!(step.code, Code::FinishOk | Code::StreamEnd) {
                return;
            }
            if step.written > 0 && !handle.put(&out[..step.written]) {
                return;
            }
            if step.code == Code::StreamEnd {
                return;
            }
        }
    }
}

impl StreamOps for BzFile {
    fn label(&self) -> &'static str {
        "BZip2"
    }

    /// `php_bz2iop_write`: `BZ2_bzwrite` in pieces of at most `INT_MAX`,
    /// what was written before a failure, or the failure.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        let mut wrote = 0;
        while wrote < buf.len() {
            let chunk = (buf.len() - wrote).min(i32::MAX as usize);
            let Some(just) = self.bzwrite(req, &buf[wrote..wrote + chunk]) else {
                return (wrote > 0).then_some(wrote);
            };
            if just == 0 {
                break;
            }
            wrote += just;
        }
        Some(wrote)
    }

    /// `php_bz2iop_read`: `BZ2_bzread` in pieces of at most `INT_MAX`
    /// until `buf` is full, `eof` set at the stream's end or an error, as
    /// #72613 has it, and the error answered where nothing was read.
    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        let mut read = 0;
        while read < buf.len() {
            let chunk = (buf.len() - read).min(i32::MAX as usize);
            match self.bzread(req, &mut buf[read..read + chunk]) {
                Some(0) => {
                    *eof = true;
                    break;
                }
                Some(just) => read += just,
                None => {
                    *eof = true;
                    return (read > 0).then_some(read);
                }
            }
        }
        Some(read)
    }

    /// `php_bz2iop_close`: `BZ2_bzclose`, which finishes a written stream
    /// and closes the file, then the stream under it.
    fn close(&mut self, req: &mut Request) {
        self.finish(req);
        self.direction = None;
        let state = req.state::<State>();
        state.errors.remove(&self.id);
        state.streams.retain(|_, id| *id != self.id);
        if let Some(handle) = self.handle.take() {
            handle.close(req);
        }
    }
}

/// `php_stream_bz2open_from_BZFILE`: a `BZip2` stream in `mode` over
/// `handle`, its file numbered in the request's state.
fn from_file(
    req: &mut Request,
    handle: Handle,
    mode: &[u8],
    direction: Direction,
) -> Result<Owner<Stream>, Thrown> {
    let state = req.state::<State>();
    let id = FileId(state.next);
    state.next += 1;
    state.errors.insert(id, Code::Ok);
    let kind = BzFile {
        id,
        handle: Some(handle),
        failed: false,
        direction: Some(direction),
    };
    let stream = req.new_stream(Box::new(kind), mode)?;
    req.state::<State>()
        .streams
        .insert(stream.resource().id(), id);
    Ok(stream)
}

/// `_php_stream_bz2open`: a `mode` of more than one character that starts
/// with neither `r` nor `w` refused; the path after `compress.bzip2://`
/// held to the `open_basedir` guard, which warns, and opened directly as `bzopen_or_bzdopen` reads `mode`, `rb` or `wb`, and
/// where that fails opened as a php stream in `mode` and cast to its fd,
/// as php's `BZ2_bzdopen` needs one; a `BZip2` stream over the one that
/// opened. The file is named, where the caller asks, by the path as
/// written when it opened directly, and by the name the inner open sets
/// otherwise, so an include through the wrapper is named as the file it
/// read.
///
/// ```php
/// $d = sys_get_temp_dir() . '/bz2-include-' . getmypid();
/// mkdir($d);
/// file_put_contents("$d/a.php", bzcompress('<?php return __FILE__;'));
/// echo str_replace($d, 'D', include "compress.bzip2://$d/./a.php"), "\n";
/// echo str_replace($d, 'D', get_included_files()[1]), "\n";
/// unlink("$d/a.php");
/// rmdir($d);
/// ```
/// ```expect
/// D/./a.php
/// D/./a.php
/// ```
///
/// A path the system cannot open directly, a `file://` URL, is opened
/// as a php stream, and the `BZip2` stream reads and writes a copy of
/// its fd, as 8.5.12's `php_bz2_bzdopen` hands libbz2 one:
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'bz');
/// var_dump(file_put_contents("compress.bzip2://file://$f", str_repeat('ab', 50)));
/// var_dump(file_get_contents("compress.bzip2://file://$f"), bzdecompress(file_get_contents($f)) === str_repeat('ab', 50));
/// unlink($f);
/// ```
/// ```expect
/// int(100)
/// string(100) "abababababababababababababababababababababababababababababababababababababababababababababababababab"
/// bool(true)
/// ```
fn open(
    req: &mut Request,
    path: &[u8],
    mode: &[u8],
    opening: Opening,
) -> Result<Option<Owner<Stream>>, Thrown> {
    let prefix = b"compress.bzip2://";
    let path = if path.len() >= prefix.len() && path[..prefix.len()].eq_ignore_ascii_case(prefix) {
        &path[prefix.len()..]
    } else {
        path
    };
    if mode.is_empty() || (mode[0] != b'w' && mode[0] != b'r' && mode.len() > 1) {
        return Ok(None);
    }
    let Ok(host) = req.environment_mut().check_open_basedir(path, true) else {
        return Ok(None);
    };
    let parsed = Mode::parse(mode);
    let handle = if let Some(file) = parsed.open(&host) {
        // `*opened_path = zend_string_init(path_copy, ...)`: the path as
        // written, where the caller holds one, as an include does.
        if opening.for_zend_stream {
            req.set_opened_path(path);
        }
        Handle::file(file)
    } else {
        let Some(inner) = req.open_stream_with(path, mode, opening)? else {
            return Ok(None);
        };
        if inner.cast(req, Cast::Fd, true).is_none() {
            inner.free(req);
            return Ok(None);
        }
        match Handle::over(req, inner) {
            Ok(handle) => handle,
            Err(inner) => {
                inner.free(req);
                return Ok(None);
            }
        }
    };
    let Some(direction) = parsed.direction() else {
        // `php_stream_close(stream)`: what this open opened, given up.
        handle.close(req);
        return Ok(None);
    };
    from_file(req, handle, mode, direction).map(Some)
}

/// `php_stream_bzip2_wrapper`, `compress.bzip2`, labelled `BZip2`: an
/// opener and nothing else.
///
/// ```php
/// $f = 'bz2-wrapper-witness.bz2';
/// var_dump(file_put_contents("compress.bzip2://$f", str_repeat("line\n", 1000)));
/// $raw = file_get_contents($f);
/// var_dump(strlen($raw), md5($raw), $raw === bzcompress(str_repeat("line\n", 1000), 9, 30));
/// var_dump(strlen(file_get_contents("compress.bzip2://$f")));
/// $h = fopen("COMPRESS.BZIP2://$f", 'r');
/// $m = stream_get_meta_data($h);
/// var_dump($m['wrapper_type'], $m['stream_type'], $m['mode'], $m['seekable'], fgets($h), ftell($h), fseek($h, 0));
/// fclose($h);
/// var_dump(@fopen("compress.bzip2://$f", 'xy'), fopen("compress.bzip2://$f", 'a') !== false);
/// var_dump(@file_exists("compress.bzip2://$f"), @unlink("compress.bzip2://$f"));
/// var_dump(fopen('compress.bzip2://php://memory', 'r'));
/// var_dump(@fopen('compress.bzip2://no-such-file.bz2', 'r'));
/// unlink($f);
/// ```
/// ```expect
/// int(5000)
/// int(53)
/// string(32) "8664326cd6225263adb6e0d0e7b78410"
/// bool(true)
/// int(5000)
///
/// Warning: fseek(): Stream does not support seeking in %s on line 9
/// string(5) "BZip2"
/// string(5) "BZip2"
/// string(1) "r"
/// bool(false)
/// string(5) "line
/// "
/// int(5)
/// int(-1)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
///
/// Warning: fopen(): Cannot represent a stream of type MEMORY as a File Descriptor in %s on line 13
///
/// Warning: fopen(compress.bzip2://php://memory): Failed to open stream: operation failed in %s on line 13
/// bool(false)
/// bool(false)
/// ```
pub(crate) fn wrapper() -> StreamWrapper {
    StreamWrapper {
        scheme: "compress.bzip2".into(),
        label: "BZip2",
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

/// `stream->mode`, as `stream_get_meta_data` reads it out.
fn stream_mode(req: &mut Request, stream: Stream) -> Result<Vec<u8>, Thrown> {
    let meta = stream.meta_data(req)?;
    let mode = meta
        .find(b"mode")
        .and_then(|v| v.deref().as_str_bytes().map(<[u8]>::to_vec))
        .unwrap_or_default();
    meta.give_up(req);
    Ok(mode)
}

/// `bzopen($file, string $mode)`, the stream as a resource or `false`:
/// a file name opened as the wrapper opens it, under `REPORT_ERRORS`; or
/// a stream opened in `r`, `rb`, `w`, `a` or `x` with `b` at most, cast
/// to its fd and read or written through; `$mode` is `r` or `w`.
///
/// ```php
/// $f = 'bzopen-witness.bz2';
/// $h = bzopen($f, 'w');
/// var_dump(bzwrite($h, str_repeat('abc', 10)), bzwrite($h, "\nend", 2), bzwrite($h, ''), bzflush($h), bzclose($h));
/// var_dump(bin2hex(file_get_contents($f)));
/// $h = bzopen($f, 'r');
/// var_dump(get_resource_type($h), stream_get_meta_data($h)['stream_type'], bzread($h, 0), bzread($h, 4), bzread($h), bzread($h), feof($h), bzclose($h));
/// $fp = fopen($f, 'rb');
/// $h = bzopen($fp, 'r');
/// var_dump(bzread($h, 7), bzclose($h), $fp);
/// foreach (['', 'x', 'rw'] as $m) {
///     try { bzopen($f, $m); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// try { bzopen('', 'r'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { bzopen("a\0b", 'r'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { bzopen(1.5, 'r'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(bzopen('no-such-file.bz2', 'r'));
/// var_dump(bzopen(fopen($f, 'r'), 'w'), bzopen(fopen($f, 'a'), 'r'), bzopen(fopen($f, 'r+'), 'r'), bzopen(fopen('php://memory', 'r'), 'r'));
/// unlink($f);
/// ```
/// ```expect
/// int(30)
/// int(2)
/// int(0)
/// bool(true)
/// bool(true)
/// string(90) "425a683931415926535946ac58ed000005410000103a00200030cd009236948348241c2ee48a70a1208d58b1da"
/// string(6) "stream"
/// string(5) "BZip2"
/// string(0) ""
/// string(4) "abca"
/// string(28) "bcabcabcabcabcabcabcabcabc
/// e"
/// string(0) ""
/// bool(true)
/// bool(true)
/// string(7) "abcabca"
/// bool(true)
/// resource(8) of type (Unknown)
/// bzopen(): Argument #2 ($mode) must be either "r" or "w"
/// bzopen(): Argument #2 ($mode) must be either "r" or "w"
/// bzopen(): Argument #2 ($mode) must be either "r" or "w"
/// bzopen(): Argument #1 ($file) must not be empty
/// bzopen(): Argument #1 ($file) must not contain null bytes
/// bzopen(): Argument #1 ($file) must be of type string or file-resource, float given
///
/// Warning: bzopen(no-such-file.bz2): Failed to open stream: No such file or directory in %s on line 17
/// bool(false)
///
/// Warning: bzopen(): cannot write to a stream opened in read only mode in %s on line 18
///
/// Warning: bzopen(): Cannot read from a stream opened in write only mode in %s on line 18
///
/// Warning: bzopen(): Cannot use stream opened in mode 'r+' in %s on line 18
///
/// Warning: bzopen(): Cannot represent a stream of type MEMORY as a File Descriptor in %s on line 18
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn bzopen(
    req: &mut Request,
    file: Untyped<Value>,
    mode: PhpString,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    let Untyped(file) = file;
    let mode = mode.as_bytes();
    if mode != b"r" && mode != b"w" {
        return Err(arg_value_error(req, 2, "be either \"r\" or \"w\""));
    }
    let opened = match file.deref().as_ref() {
        ValueRef::Str(name) => {
            let name = name.as_bytes();
            if name.is_empty() {
                return Err(arg_value_error(req, 1, "not be empty"));
            }
            if name.contains(&0) {
                return Err(arg_type_error(req, 1, "not contain null bytes"));
            }
            let opening = Opening {
                warn: true,
                ..Opening::default()
            };
            open(req, name, mode, opening)?
        }
        ValueRef::Resource(_) => {
            let Some(stream) = Stream::from_value(req, &file) else {
                return Err(type_error(
                    req,
                    "bzopen(): supplied resource is not a valid stream resource",
                ));
            };
            open_stream(req, stream, mode)?
        }
        _ => {
            let tail = format!(
                "be of type string or file-resource, {} given",
                value_name(req, &file)
            );
            return Err(arg_type_error(req, 1, &tail));
        }
    };
    Ok(Untyped(opened.map_or(OrFalse::False, |stream| {
        OrFalse::Value(stream.into_resource())
    })))
}

/// `bzopen` over a stream: its `stream->mode` held to what `mode` needs, php's
/// warnings where it is not, then a copy of its fd, which `BZ2_bzdopen`
/// opens. So the stream may be closed first, and the `BZip2` stream reads
/// and writes its own fd still, never the next file opened under the
/// number (8.5.12's `php_bz2_bzdopen`):
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'bz');
/// $fp = fopen($f, 'w'); $bz = bzopen($fp, 'w');
/// var_dump(bzwrite($bz, 'payload'), fclose($fp));
/// $other = fopen($f . '.other', 'w'); bzclose($bz); fclose($other);
/// var_dump(bzdecompress(file_get_contents($f)), filesize($f . '.other'));
/// $fp = fopen($f, 'r'); $bz = bzopen($fp, 'r');
/// var_dump(fclose($fp), bzread($bz));
/// bzclose($bz); unlink($f); unlink($f . '.other');
/// ```
/// ```expect
/// int(7)
/// bool(true)
/// string(7) "payload"
/// int(0)
/// bool(true)
/// string(7) "payload"
/// ```
fn open_stream(
    req: &mut Request,
    stream: Stream,
    mode: &[u8],
) -> Result<Option<Owner<Stream>>, Thrown> {
    let stream_mode = stream_mode(req, stream)?;
    let primary = match stream_mode.as_slice() {
        [only] => Some(*only),
        [b'b', primary] | [primary, b'b'] => Some(*primary),
        _ => None,
    };
    let Some(primary) = primary else {
        let text = format!(
            "Cannot use stream opened in mode '{}'",
            String::from_utf8_lossy(&stream_mode)
        );
        docref(req, Level::Warning, &text);
        return Ok(None);
    };
    if mode == b"r" && primary != b'r' {
        docref(
            req,
            Level::Warning,
            "Cannot read from a stream opened in write only mode",
        );
        return Ok(None);
    }
    if mode == b"w" && !b"wax".contains(&primary) {
        docref(
            req,
            Level::Warning,
            "cannot write to a stream opened in read only mode",
        );
        return Ok(None);
    }
    if stream.cast(req, Cast::Fd, true).is_none() {
        return Ok(None);
    }
    let Some(direction) = Mode::parse(mode).direction() else {
        return Ok(None);
    };
    // `php_bz2_bzdopen`'s `dup` failing is `RETURN_FALSE`.
    let held = Owner::take(req, stream);
    let handle = match Handle::over(req, held) {
        Ok(handle) => handle,
        Err(held) => {
            held.give_up(req);
            return Ok(None);
        }
    };
    from_file(req, handle, mode, direction).map(Some)
}

/// `bzread($bz, int $length = 1024): string|false`: up to `$length`
/// bytes, `php_stream_read_to_str`.
///
/// ```php
/// $h = fopen('php://memory', 'r+');
/// try { bzread($h, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// fwrite($h, 'plain'); rewind($h);
/// var_dump(bzread($h, 3), bzread($h));
/// ```
/// ```expect
/// bzread(): Argument #2 ($length) must be greater than or equal to 0
/// string(3) "pla"
/// string(2) "in"
/// ```
#[php_function]
fn bzread(
    req: &mut Request,
    bz: Stream,
    #[php(default = 1024)] length: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Ok(length) = usize::try_from(length) else {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    };
    Ok(bz.read(req, length)?.map_or(OrFalse::False, OrFalse::Value))
}

/// `bzwrite($bz, string $data, ?int $length = null): int|false`,
/// `fwrite`'s body under its own declaration.
///
/// ```php
/// $h = fopen('php://memory', 'r+');
/// var_dump(bzwrite($h, 'abcdef', 3), bzwrite($h, 'xyz', -1), stream_get_contents($h, -1, 0));
/// ```
/// ```expect
/// int(3)
/// int(0)
/// string(3) "abc"
/// ```
#[php_function]
fn bzwrite(
    req: &mut Request,
    bz: Stream,
    data: PhpString,
    #[php(default = null)] length: Option<i64>,
) -> OrFalse<i64> {
    php_ext_standard::fwrite(req, bz, data, length)
}

/// `bzflush($bz): bool`, `fflush`'s body: `php_stream_flush`, which a
/// `BZip2` stream answers with `BZ2_bzflush`'s nothing.
///
/// ```php
/// $h = fopen('php://memory', 'r+');
/// var_dump(bzflush($h));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn bzflush(req: &mut Request, bz: Stream) -> bool {
    bz.flush(req)
}

/// `bzclose($bz): bool`, `fclose`'s body under its own declaration.
///
/// ```php
/// $h = fopen('php://memory', 'r');
/// var_dump(bzclose($h));
/// try { bzclose($h); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bzclose(): Argument #1 ($bz) must be an open stream resource
/// ```
#[php_function]
fn bzclose(req: &mut Request, bz: Stream) -> bool {
    php_ext_standard::fclose(req, bz)
}

/// `php_bz2_error`: the last error of a `BZip2` stream's file,
/// `BZ2_bzerror`, which reads a positive code as `BZ_OK`; any other
/// stream is refused.
fn last_error(req: &mut Request, bz: Stream) -> Result<Code, Thrown> {
    let state = req.state::<State>();
    let Some(id) = state.streams.get(&bz.resource().id()).copied() else {
        return Err(arg_type_error(req, 1, "be a bz2 stream"));
    };
    let code = state.errors.get(&id).copied().unwrap_or(Code::Ok);
    Ok(if code.number() > 0 { Code::Ok } else { code })
}

/// `bzerrorstrings`, indexed by the negated code.
fn error_text(code: Code) -> &'static str {
    match code {
        Code::SequenceError => "SEQUENCE_ERROR",
        Code::ParamError => "PARAM_ERROR",
        Code::MemError => "MEM_ERROR",
        Code::DataError => "DATA_ERROR",
        Code::DataErrorMagic => "DATA_ERROR_MAGIC",
        Code::IoError => "IO_ERROR",
        Code::UnexpectedEof => "UNEXPECTED_EOF",
        Code::OutbuffFull => "OUTBUFF_FULL",
        Code::ConfigError => "CONFIG_ERROR",
        Code::Ok | Code::RunOk | Code::FlushOk | Code::FinishOk | Code::StreamEnd => "OK",
    }
}

/// `bzerrno($bz): int`.
///
/// ```php
/// $f = 'bzerrno-witness.bz2';
/// file_put_contents($f, 'BZh91AY&SY' . str_repeat('x', 40));
/// $h = bzopen($f, 'r');
/// var_dump(bzerrno($h), bzread($h), bzerrno($h), bzerrstr($h), bzerror($h));
/// $w = bzopen($f, 'w');
/// var_dump(bzread($w), bzerror($w));
/// bzclose($w);
/// file_put_contents($f, substr(bzcompress(str_repeat('abc', 100)), 0, 30));
/// $h = bzopen($f, 'r');
/// var_dump(bzread($h), bzerror($h));
/// file_put_contents($f, bzcompress('done'));
/// $h = bzopen($f, 'r');
/// var_dump(bzread($h), bzerror($h), bzread($h), bzerror($h));
/// $p = fopen($f, 'r');
/// try { bzerrno($p); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// unlink($f);
/// ```
/// ```expect
/// int(0)
/// bool(false)
/// int(-4)
/// string(10) "DATA_ERROR"
/// array(2) {
///   ["errno"]=>
///   int(-4)
///   ["errstr"]=>
///   string(10) "DATA_ERROR"
/// }
/// bool(false)
/// array(2) {
///   ["errno"]=>
///   int(-1)
///   ["errstr"]=>
///   string(14) "SEQUENCE_ERROR"
/// }
/// bool(false)
/// array(2) {
///   ["errno"]=>
///   int(-7)
///   ["errstr"]=>
///   string(14) "UNEXPECTED_EOF"
/// }
/// string(4) "done"
/// array(2) {
///   ["errno"]=>
///   int(0)
///   ["errstr"]=>
///   string(2) "OK"
/// }
/// string(0) ""
/// array(2) {
///   ["errno"]=>
///   int(0)
///   ["errstr"]=>
///   string(2) "OK"
/// }
/// bzerrno(): Argument #1 ($bz) must be a bz2 stream
/// ```
#[php_function]
fn bzerrno(req: &mut Request, bz: Stream) -> Result<i64, Thrown> {
    Ok(last_error(req, bz)?.number())
}

/// `bzerrstr($bz): string`.
///
/// ```php
/// $p = fopen('php://memory', 'r');
/// try { bzerrstr($p); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bzerrstr(): Argument #1 ($bz) must be a bz2 stream
/// ```
#[php_function]
fn bzerrstr(req: &mut Request, bz: Stream) -> Result<Owner<PhpString>, Thrown> {
    let code = last_error(req, bz)?;
    PhpString::new(req, error_text(code).as_bytes())
}

/// `bzerror($bz): array`, `errno` and `errstr`.
///
/// ```php
/// $p = fopen('php://memory', 'r');
/// try { bzerror($p); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bzerror(): Argument #1 ($bz) must be a bz2 stream
/// ```
#[php_function]
fn bzerror(req: &mut Request, bz: Stream) -> Result<Owner<PhpArray>, Thrown> {
    let code = last_error(req, bz)?;
    let mut array = PhpArray::new(req)?;
    array.set_str(req, b"errno", Owner::int(code.number()))?;
    let text = PhpString::new(req, error_text(code).as_bytes())?;
    array.set_str(req, b"errstr", text.into_value())?;
    Ok(array)
}
