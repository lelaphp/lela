//! `zip_stream.c`: an entry read as a stream (`php_stream_zipio_ops`,
//! labelled `zip`), seekable where libzip's `zip_file_is_seekable` is, and
//! the `zip://archive#entry` wrapper that opens one, the `password`
//! `stream_context_create()` gives under `zip` the archive's.

use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{
    MAXPATHLEN, Opening, OwnedStream as _, Request, Stream, StreamOps, StreamWrapper, Streams as _,
    Whence,
};
use php_ext::values::Owner;

use crate::archive::{Archive, CREATE};
use crate::object::{basedir_refuses, expand};
use crate::read::{Reader, Stdio};

/// `struct php_zip_stream_data_t`: the opened entry, read as the
/// stream is read, and the stat `php_zip_ops_stat` answers for a stream
/// the wrapper opened.
struct ZipStream {
    zf: Reader,
    stat: Option<(u64, i64, bool)>,
    /// The archive a `getStream` stream reads, by its number.
    archive: Option<u64>,
    /// The `FILE` of the archive the wrapper opened, which the stream
    /// keeps (`self->za`).
    file: Option<Stdio>,
}

impl StreamOps for ZipStream {
    fn label(&self) -> &'static str {
        "zip"
    }

    /// `php_zip_ops_write`: taken and dropped.
    fn write(&mut self, _req: &mut Request, buf: &[u8]) -> Option<usize> {
        Some(buf.len())
    }

    /// `php_zip_ops_read`: `zip_fread`, `eof` at a short read, and php's
    /// warning for the error a read meets.
    ///
    /// A large entry is read a buffer at a time, and a CRC error is met
    /// at the read that reaches the end:
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('read-witness.zip', ZipArchive::CREATE|ZipArchive::OVERWRITE);
    /// $z->addFromString('big', str_repeat(hash('sha256', 'x'), 204800)); $z->close();
    /// $z->open('read-witness.zip'); $m = memory_get_usage();
    /// $f = $z->getStream('big'); $s = fread($f, 100);
    /// var_dump(strlen($s), memory_get_usage() - $m < 100000, strlen(stream_get_contents($f)));
    /// fclose($f); $z->close();
    /// $b = file_get_contents('read-witness.zip'); $c = strpos($b, "PK\x01\x02") + 16;
    /// $b[$c] = chr(ord($b[$c]) ^ 1); file_put_contents('read-witness.zip', $b);
    /// $z->open('read-witness.zip'); $f = $z->getStream('big');
    /// var_dump(strlen(fread($f, 8192)), feof($f));
    /// $n = 0; while (!feof($f)) { $n += strlen((string) fread($f, 8192)); } var_dump($n);
    /// var_dump(strlen($z->getFromName('big', 10)), strlen($z->getFromName('big')));
    /// $z->close(); unlink('read-witness.zip');
    /// ```
    /// ```expect
    /// int(100)
    /// bool(true)
    /// int(13107100)
    /// int(8192)
    /// bool(false)
    ///
    /// Warning: fread(): Zip stream error: CRC error in %s on line %d
    /// int(13099008)
    /// int(10)
    /// int(13107200)
    /// ```
    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        if self
            .archive
            .is_some_and(|id| req.state::<crate::State>().closed.contains(&id))
        {
            self.zf = Reader::closed();
        }
        let mut file = match self.archive {
            Some(id) => crate::object::file(req, id),
            None => self.file.take(),
        };
        let got = self.zf.read_through(buf.len(), &mut file);
        match self.archive {
            Some(id) => crate::object::file_back(req, id, file),
            None => self.file = file,
        }
        match got {
            Ok(data) => {
                let n = data.len();
                buf[..n].copy_from_slice(&data);
                if n == 0 || n < buf.len() {
                    *eof = true;
                }
                Some(n)
            }
            Err(status) => {
                *eof = true;
                let text = status.text(req);
                docref(req, Level::Warning, &format!("Zip stream error: {text}"));
                None
            }
        }
    }

    fn close(&mut self, _req: &mut Request) {}

    /// `php_zip_ops_seek`: `zip_fseek` within the entry, the position
    /// `zip_ftell` then reads, `-1` once a seek has failed, after which
    /// every read fails too.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('seek-witness.zip', ZipArchive::CREATE|ZipArchive::OVERWRITE);
    /// $z->addFromString('s', 'abcdefghij'); $z->setCompressionName('s', ZipArchive::CM_STORE);
    /// $z->addFromString('d', str_repeat('hello world ', 1000)); $z->close(); $z->open('seek-witness.zip');
    /// $f = $z->getStream('s');
    /// var_dump(fseek($f, 4), fread($f, 3), ftell($f), fseek($f, -2, SEEK_END), fread($f, 5), fseek($f, 1), fread($f, 2));
    /// var_dump(fseek($f, 50), ftell($f), fread($f, 3), fseek($f, 0));
    /// $f = $z->getStream('d'); var_dump(@fseek($f, 4), strlen(fread($f, 100)));
    /// $z->close(); unlink('seek-witness.zip');
    /// ```
    /// ```expect
    /// int(0)
    /// string(3) "efg"
    /// int(7)
    /// int(0)
    /// string(2) "ij"
    /// int(0)
    /// string(2) "bc"
    ///
    /// Warning: fread(): Zip stream error: Invalid argument in %s on line %d
    /// int(-1)
    /// bool(false)
    /// bool(false)
    /// int(-1)
    /// int(-1)
    /// int(100)
    /// ```
    fn seek(
        &mut self,
        _req: &mut Request,
        offset: i64,
        whence: Option<Whence>,
        position: &mut i64,
    ) -> Option<bool> {
        if !self.zf.seekable() {
            return None;
        }
        let whence = match whence {
            Some(Whence::Cur) => 1,
            Some(Whence::End) => 2,
            _ => 0,
        };
        let done = self.zf.seek(offset, whence);
        *position = self.zf.tell();
        Some(done)
    }

    fn seekable(&self) -> bool {
        self.zf.seekable()
    }

    /// `php_zip_ops_stat`: the entry's size and time, a file or, by a
    /// path ending in `/`, a directory; nothing for a stream with no
    /// `#` in its path.
    fn stat(&mut self, _req: &mut Request) -> Option<[i64; 13]> {
        let (size, mtime, dir) = self.stat?;
        let mut sb = [0i64; 13];
        sb[1] = -1;
        sb[2] = if dir { 0o040_000 } else { 0o100_000 };
        sb[3] = 1;
        sb[6] = -1;
        sb[7] = if dir {
            0
        } else {
            i64::try_from(size).unwrap_or(i64::MAX)
        };
        sb[8] = mtime;
        sb[9] = mtime;
        sb[10] = mtime;
        sb[11] = -1;
        sb[12] = -1;
        Some(sb)
    }
}

/// `php_stream_zip_open`: a read stream over an opened entry; `None`
/// where `$mode` does not start with `r`.
pub(crate) fn open_entry(
    req: &mut Request,
    zf: Reader,
    orig: Option<(&[u8], u64)>,
    mode: &[u8],
    stat: Option<(u64, i64, bool)>,
    file: Option<Stdio>,
) -> Result<Option<Owner<Stream>>, Thrown> {
    if mode.first() != Some(&b'r') {
        return Ok(None);
    }
    let kind = ZipStream {
        zf,
        stat,
        archive: orig.map(|(_, id)| id),
        file,
    };
    let stream = req.new_stream(Box::new(kind), mode)?;
    if let Some((name, _)) = orig
        && let Err(thrown) = stream.set_orig_path(req, name)
    {
        stream.free(req);
        return Err(thrown);
    }
    Ok(Some(stream))
}

/// `php_stream_zip_opener`: `zip://archive#entry`, the archive opened
/// as `ZIP_CREATE` opens it and the entry by its exact name.
fn open(
    req: &mut Request,
    path: &[u8],
    mode: &[u8],
    _opening: Opening,
) -> Result<Option<Owner<Stream>>, Thrown> {
    let Some(hash) = path.iter().position(|b| *b == b'#') else {
        return Ok(None);
    };
    let fragment_len = path.len() - hash;
    let path = if path.len() >= 6 && path[..6].eq_ignore_ascii_case(b"zip://") {
        &path[6..]
    } else {
        path
    };
    if path.len() >= MAXPATHLEN || mode.first() != Some(&b'r') || path.len() < fragment_len {
        return Ok(None);
    }
    let dirname = &path[..path.len() - fragment_len];
    let fragment = &path[path.len() - fragment_len + 1..];
    if basedir_refuses(req, dirname) {
        return Ok(None);
    }
    let Some(resolved) = expand(req, dirname) else {
        return Ok(None);
    };
    let host = {
        use std::os::unix::ffi::OsStrExt;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&resolved))
    };
    let Ok(mut archive) = Archive::open(host, CREATE) else {
        return Ok(None);
    };
    if let Some(context) = req.stream_ctx()
        && let Some(password) = context.option(req, b"zip", b"password")
    {
        match password.as_str_bytes() {
            Some(p) => {
                let p = p.split(|b| *b == 0).next().unwrap_or_default();
                archive.default_password = (!p.is_empty()).then(|| p.to_vec());
            }
            None => docref(req, Level::Warning, "Can't set zip password"),
        }
    }
    let fragment = fragment.split(|b| *b == 0).next().unwrap_or_default();
    let Ok(idx) = archive.name_locate(fragment, 0) else {
        return Ok(None);
    };
    let Ok(st) = archive.stat_index(idx, 0) else {
        return Ok(None);
    };
    let Ok(zf) = archive.fopen(idx, 0, None) else {
        return Ok(None);
    };
    let dir = path.last() == Some(&b'/');
    let file = archive.data.as_mut().and_then(|d| d.stdio.take());
    open_entry(req, zf, None, mode, Some((st.size, st.mtime, dir)), file)
}

/// `php_stream_zip_wrapper`, `zip wrapper`: an opener and nothing else.
pub(crate) fn wrapper() -> StreamWrapper {
    StreamWrapper {
        scheme: "zip".into(),
        label: "zip wrapper",
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
