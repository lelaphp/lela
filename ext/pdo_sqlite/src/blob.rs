//! `Pdo\Sqlite::openBlob()` and `php_stream_pdosqlite3_ops`: a blob of
//! one row opened as a stream, read, written in place within its size,
//! and sought, as `pdo_sqlite.c` serves it.

use php_ext::coerce::argument;
use php_ext::raising::{Level, Thrown, arg_null_deprecated, docref, warning};
use php_ext::request::{Request, StreamOps, Whence};
use php_ext::values::{Value, ValueRef};

use crate::ffi::{self, Blob};
use crate::sqlite::Handle;

/// `php_stream_pdosqlite3_data`.
struct BlobStream {
    blob: Option<Blob>,
    position: usize,
    size: usize,
    /// `flags & SQLITE_OPEN_READONLY`, the refusal php's write reads.
    read_only: bool,
}

impl StreamOps for BlobStream {
    fn label(&self) -> &'static str {
        "PDOSQLite"
    }

    /// `php_pdosqlite3_stream_write`.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        if self.read_only {
            docref(
                req,
                Level::Warning,
                "Can't write to blob stream: is open as read only",
            );
            return None;
        }
        if self.position + buf.len() > self.size {
            docref(
                req,
                Level::Warning,
                "It is not possible to increase the size of a BLOB",
            );
            return None;
        }
        if !self.blob.as_ref()?.write(buf, self.position) {
            return None;
        }
        self.position = (self.position + buf.len()).min(self.size);
        Some(buf.len())
    }

    /// `php_pdosqlite3_stream_read`.
    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        let _ = req;
        let mut count = buf.len();
        if self.position + count >= self.size {
            count = self.size - self.position;
            *eof = true;
        }
        if count > 0 {
            if !self.blob.as_ref()?.read(&mut buf[..count], self.position) {
                return None;
            }
            self.position += count;
        }
        Some(count)
    }

    /// `php_pdosqlite3_stream_close`.
    fn close(&mut self, req: &mut Request) {
        let _ = req;
        self.blob = None;
    }

    /// `php_pdosqlite3_stream_seek`.
    fn seek(
        &mut self,
        req: &mut Request,
        offset: i64,
        whence: Option<Whence>,
        position: &mut i64,
    ) -> Option<bool> {
        let _ = req;
        let size = i64::try_from(self.size).unwrap_or(i64::MAX);
        let at = i64::try_from(self.position).unwrap_or(i64::MAX);
        let (target, clamp) = match whence {
            Some(Whence::Cur) if offset < 0 && at < -offset => (None, 0),
            Some(Whence::Cur) if offset >= 0 && at + offset > size => (None, size),
            Some(Whence::Cur) => (Some(at + offset), 0),
            Some(Whence::Set) if size < offset => (None, size),
            Some(Whence::Set) => (Some(offset), 0),
            Some(Whence::End) if offset > 0 => (None, size),
            Some(Whence::End) if size < -offset => (None, 0),
            Some(Whence::End) => (Some(size + offset), 0),
            None => {
                *position = at;
                return Some(false);
            }
        };
        if let Some(target) = target {
            self.position = usize::try_from(target).unwrap_or(0);
            *position = target;
            Some(true)
        } else {
            self.position = usize::try_from(clamp).unwrap_or(0);
            *position = -1;
            Some(false)
        }
    }

    fn seekable(&self) -> bool {
        true
    }

    /// `php_pdosqlite3_stream_stat`: the size alone.
    fn size(&self) -> Option<i64> {
        i64::try_from(self.size).ok()
    }
}

/// `Pdo\Sqlite::openBlob(string $table, string $column, int $rowid, ?string $dbname = "main", int $flags = Pdo\Sqlite::OPEN_READONLY)`:
/// the blob as a stream, read only unless the bits ask to write, php's
/// warning and `false` where the library cannot open it.
///
/// ```php
/// $db = new Pdo\Sqlite('sqlite::memory:');
/// $db->exec("CREATE TABLE t (b BLOB); INSERT INTO t VALUES ('hello')");
/// $s = $db->openBlob('t', 'b', 1);
/// var_dump(stream_get_contents($s), fwrite($s, 'x'));
/// $w = $db->openBlob('t', 'b', 1, 'main', Pdo\Sqlite::OPEN_READWRITE);
/// var_dump(fwrite($w, 'J'), fwrite($w, 'too long'));
/// fclose($w);
/// var_dump($db->query('SELECT b FROM t')->fetchColumn(), $db->openBlob('t', 'b', 9));
/// ```
/// ```expect
/// Warning: fwrite(): Can't write to blob stream: is open as read only in %s on line 5
/// string(5) "hello"
/// bool(false)
///
/// Warning: fwrite(): It is not possible to increase the size of a BLOB in %s on line 7
/// int(1)
/// bool(false)
///
/// Warning: Unable to open blob: no such rowid: 9 in %s on line 9
/// string(5) "Jello"
/// bool(false)
/// ```
pub(super) fn open_blob(req: &mut Request) -> Result<Value, Thrown> {
    let this = crate::this(req)?;
    php_ext_pdo::connector::with_connection(req, this, |req, conn| {
        crate::arity(req, 3, 5)?;
        let table = crate::bytes_arg(req, 1)?;
        let column = crate::bytes_arg(req, 2)?;
        let row = crate::long_arg(req, 3, 0)?;
        let dbname = match argument(req, 4)? {
            Some(value) if matches!(value.as_ref(), ValueRef::Null) => {
                arg_null_deprecated(req, 4, "string");
                Vec::new()
            }
            Some(value) => value.as_str_bytes().map(<[u8]>::to_vec).unwrap_or_default(),
            None => b"main".to_vec(),
        };
        let flags = crate::long_arg(req, 5, i64::from(ffi::SQLITE_OPEN_READONLY))?;
        let writable = flags & i64::from(ffi::SQLITE_OPEN_READWRITE) != 0;
        let Some(handle) = (conn as &mut dyn std::any::Any).downcast_mut::<Handle>() else {
            return Ok(Value::bool(false));
        };
        let blob = match handle.db.blob_open(&dbname, &table, &column, row, writable) {
            Ok(blob) => blob,
            Err(text) => {
                let text = String::from_utf8_lossy(&text).into_owned();
                warning(req, &format!("Unable to open blob: {text}"));
                return Ok(Value::bool(false));
            }
        };
        let size = blob.len();
        let stream = BlobStream {
            blob: Some(blob),
            position: 0,
            size,
            read_only: flags & i64::from(ffi::SQLITE_OPEN_READONLY) != 0,
        };
        let mode: &[u8] = if writable { b"r+b" } else { b"rb" };
        Ok(req.new_stream(Box::new(stream), mode)?.as_value())
    })
}
