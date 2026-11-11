//! The `flatfile` handler: `dba_flatfile.c` over php's bundled
//! `libflatfile/flatfile.c`. A database is records one after another,
//! each the key's length on a line, the key, the value's length on a
//! line and the value; a deleted record's key has its first byte
//! overwritten with NUL, and `firstkey` and `nextkey` pass over it. The
//! read buffer is php's, `FLATFILE_BLOCK_SIZE` bytes of the request's
//! heap grown to a length a record claims, so a length past the memory
//! limit is php's fatal and a negative one stops the walk.

use php_ext::abi::EngineState;
use php_ext::coerce::atoi;
use php_ext::raising::{Thrown, fatal};
use php_ext::request::{Request, Stream, Whence};
use php_ext::values::Emalloc;

/// `FLATFILE_BLOCK_SIZE`.
const BLOCK: usize = 1024;

/// `atoi` narrowed to `size_t` as C converts a negative `int`.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn length(bytes: &[u8]) -> usize {
    i64::from(atoi(bytes)) as u64 as usize
}

/// `php_stream_gets(fp, buf, max)`: the next line, at most `max - 1`
/// bytes of it, `None` at the end.
pub(crate) fn gets(
    req: &mut Request,
    file: Stream,
    max: Option<usize>,
) -> Result<Option<Vec<u8>>, Thrown> {
    file.get_line(req, max)
}

/// The walk's buffer, `buf` and `buf_size` of `flatfile.c`, held while
/// the walk runs and given back at its end.
struct Buffer(Option<Emalloc>);

impl Buffer {
    fn new(req: &mut Request) -> Result<Buffer, Thrown> {
        Emalloc::new(req, BLOCK).map(|held| Buffer(Some(held)))
    }

    fn len(&self) -> usize {
        self.0.as_ref().map_or(0, Emalloc::len)
    }

    /// `FLATFILE_GROW_BUF`: the length on the line, the buffer grown to
    /// hold it; `None` where the length would overflow the growth, which
    /// stops the walk.
    fn grow(&mut self, req: &mut Request, line: &[u8]) -> Result<Option<usize>, Thrown> {
        let num = length(line);
        if num >= self.len() {
            if num > usize::MAX - BLOCK {
                return Ok(None);
            }
            if let Some(held) = self.0.take() {
                self.0 = Some(held.resize(req, num + BLOCK)?);
            }
        }
        Ok(Some(num))
    }

    /// `num = php_stream_read(fp, buf, num)`: php's `-1` is `size_t`'s
    /// largest, which no key's length equals.
    fn read(&mut self, req: &mut Request, file: Stream, num: usize) -> usize {
        match self.0.as_mut() {
            Some(held) => file
                .read_into(req, &mut held.bytes_mut()[..num])
                .unwrap_or(usize::MAX),
            None => usize::MAX,
        }
    }

    fn bytes(&self) -> &[u8] {
        self.0.as_ref().map_or(&[], Emalloc::bytes)
    }

    fn free(self, req: &mut Request) {
        if let Some(held) = self.0 {
            held.free(req);
        }
    }
}

/// One step of a walk: a length line and the bytes it counts, `None`
/// where the line or its length stops the walk.
fn field(req: &mut Request, file: Stream, buf: &mut Buffer) -> Result<Option<usize>, Thrown> {
    let Some(line) = gets(req, file, Some(15))? else {
        return Ok(None);
    };
    let Some(num) = buf.grow(req, &line)? else {
        return Ok(None);
    };
    Ok(Some(buf.read(req, file, num)))
}

/// `flatfile`: where `nextkey` goes on from, `CurrentFlatFilePos`, and
/// whether a walk is under way, `nextkey.dptr`.
#[derive(Default, EngineState)]
pub(crate) struct Flatfile {
    position: i64,
    walking: bool,
}

/// What `flatfile_store` answers: stored, the key there already for an
/// insert, or a write that fell short.
pub(crate) enum Stored {
    Done,
    Exists,
    Short,
}

impl Flatfile {
    /// `flatfile_findkey`: whether a record's key is `key`, the stream
    /// left past it.
    fn find(req: &mut Request, file: Stream, key: &[u8]) -> Result<bool, Thrown> {
        let mut buf = Buffer::new(req)?;
        file.seek(req, 0, Whence::Set);
        let mut found = false;
        while !file.eof(req) {
            let Some(num) = field(req, file, &mut buf)? else {
                break;
            };
            if num == key.len() && &buf.bytes()[..num] == key {
                found = true;
                break;
            }
            if field(req, file, &mut buf)?.is_none() {
                break;
            }
        }
        buf.free(req);
        Ok(found)
    }

    /// `flatfile_fetch`: the value of the record under `key`.
    pub(crate) fn fetch(
        req: &mut Request,
        file: Stream,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, Thrown> {
        if !Self::find(req, file, key)? {
            return Ok(None);
        }
        let Some(line) = gets(req, file, Some(16))? else {
            return Ok(None);
        };
        let size = length(&line);
        // `safe_emalloc(dsize, 1, 1)`.
        let Some(asked) = size.checked_add(1) else {
            return Err(fatal(
                req,
                &format!("Possible integer overflow in memory allocation ({size} * 1 + 1)"),
            ));
        };
        let mut value = Emalloc::new(req, asked)?;
        let read = file
            .read_into(req, &mut value.bytes_mut()[..size])
            .unwrap_or(0);
        let bytes = value.bytes()[..read].to_vec();
        value.free(req);
        Ok(Some(bytes))
    }

    /// `flatfile_store`: an insert appends unless the key is there; a
    /// replace deletes the record first and writes where that left the
    /// stream.
    pub(crate) fn store(
        req: &mut Request,
        file: Stream,
        key: &[u8],
        value: &[u8],
        insert: bool,
    ) -> Result<Stored, Thrown> {
        if insert {
            if Self::find(req, file, key)? {
                return Ok(Stored::Exists);
            }
            file.seek(req, 0, Whence::End);
        } else {
            Self::delete(req, file, key)?;
        }
        for (i, bytes) in [key, value].into_iter().enumerate() {
            file.write(req, format!("{}\n", bytes.len()).as_bytes());
            // php flushes after each length but the replace's second.
            if insert || i == 0 {
                file.flush(req);
            }
            if file.write(req, bytes).unwrap_or(0) < bytes.len() {
                return Ok(Stored::Short);
            }
        }
        file.flush(req);
        Ok(Stored::Done)
    }

    /// `flatfile_delete`: the record's key overwritten with a NUL at its
    /// first byte; `false` where no record has the key.
    pub(crate) fn delete(req: &mut Request, file: Stream, key: &[u8]) -> Result<bool, Thrown> {
        let mut buf = Buffer::new(req)?;
        file.seek(req, 0, Whence::Set);
        let mut deleted = false;
        while !file.eof(req) {
            let Some(line) = gets(req, file, Some(15))? else {
                break;
            };
            let Some(num) = buf.grow(req, &line)? else {
                break;
            };
            let pos = file.tell(req);
            let num = buf.read(req, file, num);
            if num == key.len() && &buf.bytes()[..num] == key {
                file.seek(req, pos, Whence::Set);
                file.write(req, &[0]);
                file.flush(req);
                file.seek(req, 0, Whence::End);
                deleted = true;
                break;
            }
            if field(req, file, &mut buf)?.is_none() {
                break;
            }
        }
        buf.free(req);
        Ok(deleted)
    }

    /// `flatfile_firstkey`: the first record whose key is not deleted.
    pub(crate) fn first_key(
        &mut self,
        req: &mut Request,
        file: Stream,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let mut buf = Buffer::new(req)?;
        file.seek(req, 0, Whence::Set);
        let found = loop {
            if file.eof(req) {
                break None;
            }
            let Some(num) = field(req, file, &mut buf)? else {
                break None;
            };
            if buf.bytes().first().is_some_and(|&b| b != 0) {
                break Some(self.found(req, file, &buf, num));
            }
            if field(req, file, &mut buf)?.is_none() {
                break None;
            }
        };
        buf.free(req);
        self.walking = found.is_some();
        Ok(found)
    }

    /// `flatfile_nextkey`: past the value of the key last answered, the
    /// next record whose key is not deleted; nothing once a walk ended.
    pub(crate) fn next_key(
        &mut self,
        req: &mut Request,
        file: Stream,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        if !self.walking {
            return Ok(None);
        }
        let mut buf = Buffer::new(req)?;
        file.seek(req, self.position, Whence::Set);
        let found = loop {
            if file.eof(req) {
                break None;
            }
            if field(req, file, &mut buf)?.is_none() {
                break None;
            }
            let Some(num) = field(req, file, &mut buf)? else {
                break None;
            };
            if buf.bytes().first().is_some_and(|&b| b != 0) {
                break Some(self.found(req, file, &buf, num));
            }
        };
        buf.free(req);
        self.walking = found.is_some();
        Ok(found)
    }

    /// The key a walk answers, the position past it kept.
    fn found(&mut self, req: &Request, file: Stream, buf: &Buffer, num: usize) -> Vec<u8> {
        self.position = file.tell(req);
        buf.bytes()[..num.min(buf.bytes().len())].to_vec()
    }
}

/// `flatfile_version`.
pub(crate) const VERSION: &str = "1.0, $Id: a68fed7e38c4318d938b19f5644d9962fe0beb49 $";
