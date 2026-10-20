//! `zend_file_handle` (`Z/Zend/zend_stream.h`) as
//! `php_stream_open_for_zend_ex` opens it for the engine's scanner: what
//! an `include` of a wrapper's path and `parse_ini_file()` read through.

use crate::api::raising::Thrown;
use crate::api::request::{OwnedStream, Request, Stream, StreamOption};
use crate::api::values::{Emalloc, Owner, PhpString};
use crate::streams::OpenOptions;
use crate::streams::StreamLayer;

/// `ZEND_MMAP_AHEAD`: the zero bytes `zend_stream_fixup` keeps past what
/// it read, for the scanner's look-ahead.
const ZEND_MMAP_AHEAD: usize = 32;

/// One file opened for the scanner, until [`ScannerFile::destroy`]: the
/// stream, its read buffer off; the `*opened_path` its opener set, a
/// string of the request's (the real path the plain wrapper makes, or
/// the path `include_path` resolved to); and the buffer
/// [`ScannerFile::load`] reads it into. The open's resolved path is a
/// transient string of the request's, so `memory_get_peak_usage()` sees
/// php's open as well as its read. A long name makes the open the peak,
/// found through `include_path` too:
///
/// ```php
/// $d = tempnam(sys_get_temp_dir(), 'fh'); unlink($d); mkdir($d); $p = $d . '/' . str_repeat('f', 100) . '.ini'; file_put_contents($p, "a");
/// parse_ini_file($p);
/// memory_reset_peak_usage(); $m = memory_get_usage(); $a = parse_ini_file($p);
/// var_dump(memory_get_peak_usage() - $m, memory_get_usage() - $m);
/// set_include_path($d); $a = parse_ini_file(str_repeat('f', 100) . '.ini');
/// memory_reset_peak_usage(); $m = memory_get_usage(); $a = parse_ini_file(str_repeat('f', 100) . '.ini');
/// var_dump(memory_get_peak_usage() - $m);
/// unlink($p); rmdir($d);
/// ```
/// ```expect
/// int(1176)
/// int(56)
/// int(1336)
/// ```
pub struct ScannerFile {
    stream: Owner<Stream>,
    opened_path: Option<Owner<PhpString>>,
    buf: Option<Emalloc>,
}

impl ScannerFile {
    /// `php_stream_open_for_zend_ex(handle, USE_PATH | REPORT_ERRORS |
    /// STREAM_OPEN_FOR_INCLUDE)`: the path opened along `include_path`,
    /// as an include's open, so `allow_url_include` holds a URL wrapper
    /// to it, with buffering off "to avoid double buffering between PHP
    /// and Zend streams"; `None` for the open that failed, which warned.
    /// The `Err` is what the open threw, or the memory limit's fatal.
    pub fn open(req: &mut Request, path: &[u8]) -> Result<Option<ScannerFile>, Thrown> {
        ScannerFile::open_with(
            req,
            path,
            OpenOptions {
                use_path: true,
                for_include: true,
                for_zend_stream: true,
                ..OpenOptions::REPORT
            },
        )
    }

    /// `php_fopen_primary_script`'s `zend_stream_open` of the host's
    /// program (`Z/main/fopen_wrappers.c:468-473`): [`ScannerFile::open`]
    /// under `display_errors=0`, its fd inherited by a process the
    /// request execs while the program runs, since the host's
    /// `zend_file_handle` holds it open until the program has been
    /// served or the request ends.
    pub(crate) fn open_held(req: &mut Request, path: &[u8]) -> Result<Option<ScannerFile>, Thrown> {
        ScannerFile::open_with(
            req,
            path,
            OpenOptions {
                use_path: true,
                for_include: true,
                for_zend_stream: true,
                held: true,
                ..OpenOptions::default()
            },
        )
    }

    fn open_with(
        req: &mut Request,
        path: &[u8],
        options: OpenOptions,
    ) -> Result<Option<ScannerFile>, Thrown> {
        let Some((stream, opened_path)) = req.open_wrapper_named(path, b"rb", options)? else {
            return Ok(None);
        };
        stream.set_option(
            req,
            StreamOption::ReadBuffer {
                buffered: false,
                size: crate::api::layer::BUFSIZ,
            },
        );
        Ok(Some(ScannerFile {
            stream,
            opened_path,
            buf: None,
        }))
    }

    /// The `*opened_path`, which an include compiles the file under.
    pub(crate) fn opened_path(&self) -> Option<PhpString> {
        self.opened_path.as_deref().copied()
    }

    /// `zend_stream_fixup` (`Z/Zend/zend_stream.c:131-210`): the whole
    /// stream read into the buffer php `emalloc`s, `ZEND_MMAP_AHEAD` past
    /// the size `php_zend_stream_fsizer` stats, or, where it stats none
    /// (a stream with a read filter among them), four kilobytes doubled
    /// as each fills and widened by the look-ahead where less of it is
    /// left; the bytes read, `None` for a read that failed, the buffer
    /// freed. The `Err` is the memory limit's fatal.
    pub fn load(&mut self, req: &mut Request) -> Result<Option<&[u8]>, Thrown> {
        let stream = *self.stream;
        let file_size = req
            .stream_op(stream, |req, s| s.zend_fsize(req))
            .unwrap_or(0);
        let (mut buf, len) = if file_size > 0 {
            let mut buf = Emalloc::new(req, file_size.saturating_add(ZEND_MMAP_AHEAD))?;
            let mut size = 0;
            loop {
                match stream.read_into(req, &mut buf.bytes_mut()[size..file_size]) {
                    Some(0) => break,
                    Some(n) => size += n,
                    None => {
                        buf.free(req);
                        return Ok(None);
                    }
                }
            }
            (buf, size)
        } else {
            let (mut size, mut remain) = (0, 4 * 1024);
            let mut buf = Emalloc::new(req, remain)?;
            loop {
                match stream.read_into(req, &mut buf.bytes_mut()[size..size + remain]) {
                    Some(0) => break,
                    Some(n) => {
                        size += n;
                        remain -= n;
                        if remain == 0 {
                            buf = buf.resize(req, size.saturating_mul(2))?;
                            remain = size;
                        }
                    }
                    None => {
                        buf.free(req);
                        return Ok(None);
                    }
                }
            }
            if size > 0 && remain < ZEND_MMAP_AHEAD {
                buf = buf.resize(req, size.saturating_add(ZEND_MMAP_AHEAD))?;
            }
            (buf, size)
        };
        if len == 0 {
            buf = buf.resize(req, ZEND_MMAP_AHEAD)?;
        }
        let buf = self.buf.insert(buf);
        Ok(Some(&buf.bytes()[..len]))
    }

    /// `zend_destroy_file_handle`: the stream closed, the opened path
    /// given up, then the buffer freed.
    pub fn destroy(self, req: &mut Request) {
        self.stream.free(req);
        if let Some(opened) = self.opened_path {
            opened.give_up(req);
        }
        if let Some(buf) = self.buf {
            buf.free(req);
        }
    }
}
