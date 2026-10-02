//! `zip_stream.c`: an entry read as a stream (`php_stream_zipio_ops`,
//! labelled `zip`), seekable where libzip's `zip_file_is_seekable` is, and
//! the `zip://archive#entry` wrapper that opens one, the `password`
//! `stream_context_create()` gives under `zip` the archive's.

use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Opening, Request, Stream, StreamOps, StreamWrapper, Whence};

use crate::archive::{Archive, CREATE};
use crate::error::Status;
use crate::object::{basedir_refuses, expand};

/// `struct php_zip_stream_data_t`: the entry's bytes, the cursor, what
/// reading past the last byte meets, and the stat `php_zip_ops_stat`
/// answers for a stream the wrapper opened.
struct ZipStream {
    data: Vec<u8>,
    cursor: usize,
    fault: Option<Status>,
    seekable: bool,
    stat: Option<(u64, i64, bool)>,
    /// The archive a `getStream` stream reads, by its number.
    archive: Option<u64>,
}

impl StreamOps for ZipStream {
    fn label(&self) -> &'static str {
        "zip"
    }

    /// `php_zip_ops_write`: taken and dropped.
    fn write(&mut self, _req: &mut Request, buf: &[u8]) -> Option<usize> {
        Some(buf.len())
    }

    /// `php_zip_ops_read`: `eof` at a short read, and php's warning for
    /// the error the end of the data meets.
    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        if self
            .archive
            .is_some_and(|id| req.state::<crate::State>().closed.contains(&id))
        {
            self.data.clear();
            self.fault = Some(Status::of(crate::error::Er::Zipclosed, 0));
        }
        let rest = self.data.get(self.cursor..).unwrap_or_default();
        if rest.is_empty()
            && let Some(fault) = self.fault.take()
        {
            *eof = true;
            let text = fault.text(req);
            docref(req, Level::Warning, &format!("Zip stream error: {text}"));
            return None;
        }
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        if n == 0 || n < buf.len() {
            *eof = true;
        }
        self.cursor += n;
        Some(n)
    }

    fn close(&mut self, _req: &mut Request) {}

    /// `php_zip_ops_seek`: `zip_fseek` within the entry, the position
    /// `zip_ftell` then reads.
    fn seek(
        &mut self,
        _req: &mut Request,
        offset: i64,
        whence: Option<Whence>,
        position: &mut i64,
    ) -> Option<bool> {
        if !self.seekable {
            return None;
        }
        let size = i64::try_from(self.data.len()).unwrap_or(i64::MAX);
        let base = match whence {
            Some(Whence::Cur) => i64::try_from(self.cursor).unwrap_or(i64::MAX),
            Some(Whence::End) => size,
            _ => 0,
        };
        let target = base.checked_add(offset).filter(|t| (0..=size).contains(t));
        if let Some(t) = target {
            self.cursor = usize::try_from(t).unwrap_or(0);
        }
        *position = i64::try_from(self.cursor).unwrap_or(0);
        Some(target.is_some())
    }

    fn seekable(&self) -> bool {
        self.seekable
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
    opened: crate::archive::Opened,
    orig: Option<(&[u8], u64)>,
    mode: &[u8],
    stat: Option<(u64, i64, bool)>,
) -> Result<Option<Stream>, Thrown> {
    if mode.first() != Some(&b'r') {
        return Ok(None);
    }
    let kind = ZipStream {
        data: opened.data,
        cursor: 0,
        fault: opened.fault,
        seekable: opened.seekable,
        stat,
        archive: orig.map(|(_, id)| id),
    };
    let stream = req.new_stream(Box::new(kind), mode)?;
    if let Some((name, _)) = orig {
        stream.set_orig_path(req, name)?;
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
) -> Result<Option<Stream>, Thrown> {
    let Some(hash) = path.iter().position(|b| *b == b'#') else {
        return Ok(None);
    };
    let fragment_len = path.len() - hash;
    let path = if path.len() >= 6 && path[..6].eq_ignore_ascii_case(b"zip://") {
        &path[6..]
    } else {
        path
    };
    if path.len() >= 4096 || mode.first() != Some(&b'r') || path.len() < fragment_len {
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
        && let Some(password) =
            php_ext_standard::stream_ctx_option(req, context, b"zip", b"password")?
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
    let Ok(opened) = archive.fopen(idx, 0, None) else {
        return Ok(None);
    };
    let dir = path.last() == Some(&b'/');
    open_entry(req, opened, None, mode, Some((st.size, st.mtime, dir)))
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
