//! `Pdo\Sqlite::openBlob()` and `php_stream_pdosqlite3_ops`: a blob of
//! one row opened as a stream, read, written in place within its size,
//! and sought, as `pdo_sqlite.c` serves it, and as `sqlite3.c`'s
//! `php_stream_sqlite3_ops` serves `SQLite3::openBlob()`, the same
//! operations under another label, which `ext/sqlite3` opens with
//! [`BlobStream`].

use php_ext::abi::Raw;
use php_ext::raising::{Level, Thrown, arg_count_error, arg_null_deprecated, docref, warning};
use php_ext::request::{Request, StreamOps, Streams as _, Whence};
use php_ext::values::{Owner, PhpObject, PhpString, Value, ValueRef};

use crate::ffi::{self, Blob};
use crate::sqlite::Handle;

/// `php_stream_pdosqlite3_data` and `php_stream_sqlite3_data`.
pub struct BlobStream {
    blob: Option<Blob>,
    position: usize,
    size: usize,
    /// `flags & SQLITE_OPEN_READONLY`, the refusal php's write reads.
    read_only: bool,
    /// The ops' label, `PDOSQLite` or `SQLite3`.
    label: &'static str,
}

impl BlobStream {
    /// A stream over `blob` from its first byte, refusing writes where
    /// the bits it was opened with ask to read only.
    #[must_use]
    pub fn new(blob: Blob, read_only: bool, label: &'static str) -> BlobStream {
        BlobStream {
            size: blob.size(),
            blob: Some(blob),
            position: 0,
            read_only,
            label,
        }
    }
}

impl StreamOps for BlobStream {
    fn label(&self) -> &'static str {
        self.label
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

/// `Pdo\Sqlite::openBlob()`'s body: the blob as a stream, read only
/// unless the bits ask to write, php's warning and `false` where the
/// library cannot open it. `PDO_CONSTRUCT_CHECK` first, then
/// `zend_parse_parameters(.., "ppl|pl", ..)`.
pub(super) fn open_blob(
    req: &mut Request,
    this: PhpObject,
    [table, column]: [Raw<PhpString>; 2],
    rowid: Raw<i64>,
    dbname: Raw<Option<PhpString>>,
    flags: Raw<i64>,
) -> Result<Owner<Value>, Thrown> {
    php_ext_pdo::connector::with_connection(req, this, |req, conn| {
        let given = req.frames().argument_count();
        if !(3..=5).contains(&given) {
            let bound = if given < 3 { "at least 3" } else { "at most 5" };
            return Err(arg_count_error(
                req,
                &format!("{bound} arguments, {given} given"),
            ));
        }
        let table = table.hold(req)?.as_bytes().to_vec();
        let column = column.hold(req)?.as_bytes().to_vec();
        let row = rowid.hold(req)?;
        let dbname = if matches!(dbname.value().deref().as_ref(), ValueRef::Null) {
            arg_null_deprecated(req, 4, "string");
            Vec::new()
        } else {
            dbname
                .hold(req)?
                .map(|name| name.as_bytes().to_vec())
                .unwrap_or_default()
        };
        let flags = flags.hold(req)?;
        let writable = flags & i64::from(ffi::SQLITE_OPEN_READWRITE) != 0;
        let Some(handle) = (conn as &mut dyn std::any::Any).downcast_mut::<Handle>() else {
            return Ok(Owner::bool(false));
        };
        let Some(db) = handle.db() else {
            return Ok(Owner::bool(false));
        };
        let blob = match db.blob_open(req, &dbname, &table, &column, row, writable) {
            Ok(blob) => blob,
            Err(text) => {
                let text = String::from_utf8_lossy(&text).into_owned();
                warning(req, &format!("Unable to open blob: {text}"));
                return Ok(Owner::bool(false));
            }
        };
        let read_only = flags & i64::from(ffi::SQLITE_OPEN_READONLY) != 0;
        let stream = BlobStream::new(blob, read_only, "PDOSQLite");
        let mode: &[u8] = if writable { b"r+b" } else { b"rb" };
        Ok(req
            .new_stream(Box::new(stream), mode)?
            .into_resource()
            .into_value())
    })
}
