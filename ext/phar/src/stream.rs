//! `stream.c` and `dirstream.c`: the `phar://` wrapper, registered at
//! the end of the wrapper table as `PHP_MINIT(phar)` registers it, its
//! entry streams (`phar_ops`, labelled `phar stream`), its `url_stat`,
//! and its directories (`phar_make_dirstream`), sorted as php sorts them.

use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{
    DirListing, MAXPATHLEN, Opening, OwnedStream as _, Request, Stream, StreamOps, StreamWrapper,
    Streams as _, Whence,
};
use php_ext::values::{Owner, PhpArray};

use crate::archive::{self, ArchiveId, archives, lossy};
use crate::entry::{self, Dir, EntryData, Target};
use crate::open::{self, readonly};
use crate::path::{self, Create, Exec};
use crate::write;

/// `phar_parse_url`'s `php_url`: the archive's name and the path in it.
pub(crate) struct Url {
    pub(crate) host: Vec<u8>,
    pub(crate) path: Vec<u8>,
}

/// Where `phar_parse_url`'s refusals go, `php_stream_wrapper_log_error`
/// by the caller's options: nowhere under `PHP_STREAM_URL_STAT_QUIET`, the
/// wrapper's list for an opener to show, or a warning at once under
/// `REPORT_ERRORS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Report {
    Quiet,
    Log,
    Warn,
}

/// `phar_parse_url`: `phar://` split into the archive's name and the path
/// inside it, the archive opened, or created for a write; each refusal
/// reported as `report` says.
pub(crate) fn parse_url(
    req: &mut Request,
    filename: &[u8],
    mode: &[u8],
    report: Report,
) -> Result<Option<Url>, Thrown> {
    let log = |req: &mut Request, message: String| match report {
        Report::Quiet => {}
        Report::Log => req.wrapper_log_error(&message),
        Report::Warn => docref(req, Level::Warning, &message),
    };
    if filename.len() < 7 || !filename[..7].eq_ignore_ascii_case(b"phar://") {
        return Ok(None);
    }
    if mode.first() == Some(&b'a') {
        log(req, "phar error: open mode append not supported".into());
        return Ok(None);
    }
    let create = if mode.first() == Some(&b'w') {
        Create::Either
    } else {
        Create::No
    };
    let split = match path::split_fname(req, filename, Exec::Either, create)? {
        Ok(split) => split,
        Err(failed) => {
            let message = match failed.arch {
                Some(arch) => format!(
                    "phar error: no directory in \"{}\", must have at least phar://{}/ for root directory (always use full path to a new phar)",
                    lossy(filename),
                    lossy(&arch)
                ),
                None => format!(
                    "phar error: invalid url or non-existent phar \"{}\"",
                    lossy(filename)
                ),
            };
            log(req, message);
            return Ok(None);
        }
    };
    let url = Url {
        host: split.arch,
        path: split.entry,
    };
    let opened = if mode.first() == Some(&b'w')
        || (mode.first() == Some(&b'r') && mode.get(1) == Some(&b'+'))
    {
        let data = archives(req)
            .by_fname(&url.host)
            .and_then(|id| archives(req).get(id))
            .is_some_and(|a| a.is_data);
        if readonly(req) && !data {
            log(
                req,
                "phar error: write operations disabled by the php.ini setting phar.readonly".into(),
            );
            return Ok(None);
        }
        open::open_or_create(req, &url.host, None, false, true)?
    } else {
        open::open_from_filename(req, &url.host, None, true)?
    };
    if let Err(error) = opened {
        if let Some(error) = error {
            log(req, error);
        }
        return Ok(None);
    }
    Ok(Some(url))
}

/// The stream of one open entry, `phar_ops` over a `phar_entry_data`.
struct EntryStream {
    data: Option<EntryData>,
}

impl EntryStream {
    fn data(&self) -> Option<&EntryData> {
        self.data.as_ref()
    }

    /// The size of the entry the stream reads, the end of its links'.
    fn source(&self, req: &mut Request) -> Option<(Option<usize>, u32, bool)> {
        let data = self.data()?;
        match &data.target {
            Target::Temp(entry) => Some((None, entry.uncompressed_filesize, entry.is_deleted)),
            Target::Slot(slot) => {
                let source = entry::link_source(req, data.archive, *slot)?;
                let entry = archives(req).get(data.archive)?.manifest.at(source)?;
                Some((Some(source), entry.uncompressed_filesize, entry.is_deleted))
            }
        }
    }
}

impl StreamOps for EntryStream {
    fn label(&self) -> &'static str {
        "phar stream"
    }

    /// `phar_stream_write`: into the entry's own buffer.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        write::stream_write(req, self.data.as_mut()?, buf)
    }

    /// `phar_stream_read`: at most what is left of the entry, `eof` at its
    /// end.
    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        let (source, size, deleted) = self.source(req)?;
        if deleted {
            *eof = true;
            return None;
        }
        let data = self.data.as_mut()?;
        let left = i64::from(size) - data.position;
        let want = buf.len().min(usize::try_from(left).unwrap_or(0));
        let at = data.zero + data.position;
        let got = match (&data.fp, &data.target, source) {
            (Some(efp), Target::Slot(_), Some(source)) => {
                entry::read_efp(req, data.archive, &Target::Slot(source), efp, at, want).ok()?
            }
            (Some(efp), target, _) => {
                entry::read_efp(req, data.archive, target, efp, at, want).ok()?
            }
            (None, _, _) => Vec::new(),
        };
        buf[..got.len()].copy_from_slice(&got);
        data.position += i64::try_from(got.len()).unwrap_or(0);
        *eof = data.position == i64::from(size);
        Some(got.len())
    }

    /// `phar_stream_close`: the flush php makes on every close, then the
    /// entry given back.
    fn close(&mut self, req: &mut Request) {
        let _ = self.flush(req);
        if let Some(data) = self.data.take() {
            entry::delref(req, data);
        }
    }

    /// `phar_stream_seek`: within the entry, from its start, its end or
    /// the position.
    fn seek(
        &mut self,
        req: &mut Request,
        offset: i64,
        whence: Option<Whence>,
        position: &mut i64,
    ) -> Option<bool> {
        let (_, size, _) = self.source(req)?;
        let data = self.data.as_mut()?;
        let target = match whence {
            Some(Whence::End) => data.zero.wrapping_add(i64::from(size)).wrapping_add(offset),
            Some(Whence::Cur) => data.zero.wrapping_add(data.position).wrapping_add(offset),
            Some(Whence::Set) => data.zero.wrapping_add(offset),
            None => 0,
        };
        if target > data.zero + i64::from(size) || target < data.zero {
            return Some(false);
        }
        data.position = target - data.zero;
        *position = data.position;
        Some(true)
    }

    fn seekable(&self) -> bool {
        true
    }

    /// `phar_stream_stat`: `phar_dostat` over the entry the stream holds,
    /// never as a made-up directory.
    fn stat(&mut self, req: &mut Request) -> Option<[i64; 13]> {
        let data = self.data.as_ref()?;
        let a = archives(req).get(data.archive)?;
        let entry = match &data.target {
            Target::Slot(slot) => a.manifest.at(*slot)?,
            Target::Temp(entry) => entry,
        };
        Some(dostat(a, Some(entry)))
    }

    /// `phar_stream_flush`: an entry something wrote to written out with
    /// its archive; one nothing wrote to is php's `EOF`.
    fn flush(&mut self, req: &mut Request) -> bool {
        match self.data.as_ref() {
            Some(data) => write::stream_flush(req, data).unwrap_or(false),
            None => false,
        }
    }
}

/// `phar_wrapper_open_url`: the entry the URL names opened for `mode`,
/// the stub where an include names the archive itself.
fn open_url(
    req: &mut Request,
    path: &[u8],
    mode: &[u8],
    opening: Opening,
) -> Result<Option<Owner<Stream>>, Thrown> {
    let Some(url) = parse_url(req, path, mode, Report::Log)? else {
        return Ok(None);
    };
    let internal = url.path[1..].to_vec();
    let host = url.host;
    if mode.first() == Some(&b'w') || (mode.first() == Some(&b'r') && mode.get(1) == Some(&b'+')) {
        let Some(data) = write::open_for_write(req, &host, &internal, mode)? else {
            return Ok(None);
        };
        opened_path(req, &data);
        return req
            .new_stream(Box::new(EntryStream { data: Some(data) }), mode)
            .map(Some);
    }
    let data = if internal.is_empty() && opening.for_include {
        let Ok(id) = archive::get_archive(req, &host, None) else {
            req.wrapper_log_error(&format!(
                "file {} is not a valid phar archive",
                lossy(&host)
            ));
            return Ok(None);
        };
        match stub_data(req, id)? {
            Ok(data) => {
                let fname = archives(req)
                    .get(id)
                    .map(|a| a.fname.clone())
                    .unwrap_or_default();
                req.set_opened_path(&fname);
                data
            }
            Err(error) => {
                req.wrapper_log_error(&error);
                return Ok(None);
            }
        }
    } else {
        match entry::get_entry_data(req, &host, &internal, b"r", Dir::No, false)? {
            Ok(Some(data)) => {
                if let Target::Slot(slot) = data.target
                    && !archives(req)
                        .get(data.archive)
                        .and_then(|a| a.manifest.at(slot))
                        .is_some_and(|e| e.is_crc_checked)
                {
                    let checked = entry::check_crc(req, data.archive, slot)?;
                    if let Err(error) = checked {
                        req.wrapper_log_error(&error);
                        entry::delref(req, data);
                        return Ok(None);
                    }
                }
                include_cwd(req, &data, opening);
                opened_path(req, &data);
                data
            }
            Ok(None) | Err(None) => {
                req.wrapper_log_error(&format!(
                    "phar error: \"{}\" is not a file in phar \"{}\"",
                    lossy(&internal),
                    lossy(&host)
                ));
                return Ok(None);
            }
            Err(Some(error)) => {
                req.wrapper_log_error(&error);
                return Ok(None);
            }
        }
    };
    req.new_stream(Box::new(EntryStream { data: Some(data) }), mode)
        .map(Some)
}

/// `*opened_path = "phar://<archive>/<entry>"`, the name an entry's
/// stream stands for.
fn opened_path(req: &mut Request, data: &EntryData) {
    let named = archives(req).get(data.archive).map(|a| {
        let entry = match &data.target {
            Target::Slot(slot) => a
                .manifest
                .key_at(*slot)
                .map(<[u8]>::to_vec)
                .unwrap_or_default(),
            Target::Temp(entry) => entry.filename.clone(),
        };
        [b"phar://".as_slice(), &a.fname, b"/", &entry].concat()
    });
    if let Some(named) = named {
        req.set_opened_path(&named);
    }
}

/// The stub an include of the bare archive runs: a tar or zip archive's
/// `.phar/stub.php`, or a phar's bytes before `__HALT_COMPILER();`'s end.
fn stub_data(req: &mut Request, id: ArchiveId) -> Result<Result<EntryData, String>, Thrown> {
    let Some(a) = archives(req).get(id) else {
        return Ok(Err(String::new()));
    };
    let fname = a.fname.clone();
    if a.is_tar() || a.is_zip() {
        return Ok(
            match entry::get_entry_data(req, &fname, b".phar/stub.php", b"r", Dir::No, false)? {
                Ok(Some(data)) => Ok(data),
                Ok(None) | Err(None) => Err(format!(
                    "phar error: \"\" is not a file in phar \"{}\"",
                    lossy(&fname)
                )),
                Err(Some(error)) => Err(error),
            },
        );
    }
    if !entry::open_archive_fp(req, id)? {
        return Ok(Err(format!(
            "phar error: could not reopen phar \"{}\"",
            lossy(&fname)
        )));
    }
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(Err(String::new()));
    };
    let mut stub = archive::Entry::new(b"");
    stub.is_temp_dir = true;
    stub.uncompressed_filesize = u32::try_from(a.halt_offset).unwrap_or(u32::MAX);
    stub.compressed_filesize = stub.uncompressed_filesize;
    stub.is_crc_checked = true;
    a.refcount += 1;
    let fp = a.fp.as_ref().map(archive::Fp::efp);
    Ok(Ok(EntryData {
        archive: id,
        target: Target::Temp(Box::new(stub)),
        fp,
        position: 0,
        zero: 0,
    }))
}

/// The directory an included file's relative includes are read from,
/// `PHAR_G(cwd)`, set by the first include out of an archive.
fn include_cwd(req: &mut Request, data: &EntryData, opening: Opening) {
    if !opening.for_include || req.state::<crate::State>().cwd_init {
        return;
    }
    let Target::Slot(slot) = data.target else {
        return;
    };
    let Some(a) = archives(req).get(data.archive) else {
        return;
    };
    let Some(name) = a.manifest.at(slot).map(|e| e.filename.clone()) else {
        return;
    };
    let stub = (a.is_tar() || a.is_zip()) && name == b".phar/stub.php";
    let state = req.state::<crate::State>();
    state.cwd_init = !stub;
    if !stub {
        state.cwd = name
            .iter()
            .rposition(|b| *b == b'/')
            .map(|cut| name[..cut].to_vec());
    }
}

/// `phar_dostat` as `php_fstat` lays it out: a file's size, `st_mode` and
/// time, a directory's, or for a directory made up for a path the
/// archive's latest time; the write bits taken off where the archive is
/// not writable; the inode the entry's name hashes to.
pub(crate) fn dostat(archive: &archive::Archive, entry: Option<&archive::Entry>) -> [i64; 13] {
    let mut sb = [0_i64; 13];
    if let Some(e) = entry {
        sb[7] = if e.is_dir {
            0
        } else {
            i64::from(e.uncompressed_filesize)
        };
        sb[2] = i64::from(e.flags & archive::ENT_PERM_MASK)
            | if e.is_dir { 0o040_000 } else { 0o100_000 };
        for t in [8, 9, 10] {
            sb[t] = i64::from(e.timestamp);
        }
        sb[1] = i64::from(e.inode);
    } else {
        sb[2] = 0o777 | 0o040_000;
        for t in [8, 9, 10] {
            sb[t] = i64::from(archive.max_timestamp);
        }
    }
    if !archive.is_writeable {
        sb[2] = (sb[2] & 0o555) | (sb[2] & !0o777);
    }
    sb[3] = 1;
    sb[6] = -1;
    sb[0] = 0xc;
    sb[11] = -1;
    sb[12] = -1;
    sb
}

/// The field names `php_fstat` gives a stat's thirteen.
const STAT_NAMES: [&[u8]; 13] = [
    b"dev", b"ino", b"mode", b"nlink", b"uid", b"gid", b"rdev", b"size", b"atime", b"mtime",
    b"ctime", b"blksize", b"blocks",
];

/// A stat as the array the engine reads an extension's `url_stat` from.
fn stat_array(req: &mut Request, sb: [i64; 13]) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for (name, field) in STAT_NAMES.iter().zip(sb) {
        out.set_str(req, name, Owner::int(field))?;
    }
    Ok(out)
}

/// `phar_wrapper_stat`: an entry, a directory under which entries stand,
/// the archive's root, or a file of a mounted directory, mounted as it is
/// found.
fn url_stat(req: &mut Request, url: &[u8]) -> Result<Option<Owner<PhpArray>>, Thrown> {
    let Some(sb) = stat(req, url)? else {
        return Ok(None);
    };
    stat_array(req, sb).map(Some)
}

/// The stat [`url_stat`] answers, as a stat.
pub(crate) fn stat(req: &mut Request, url: &[u8]) -> Result<Option<[i64; 13]>, Thrown> {
    let Some(url) = parse_url(req, url, b"r", Report::Quiet)? else {
        return Ok(None);
    };
    let Ok(id) = archive::get_archive(req, &url.host, None) else {
        return Ok(None);
    };
    let internal = &url.path[1..];
    let Some(a) = archives(req).get(id) else {
        return Ok(None);
    };
    if internal.is_empty() {
        return Ok(Some(dostat(a, None)));
    }
    if let Some(e) = a.manifest.get(internal) {
        return Ok(Some(dostat(a, Some(e))));
    }
    if a.virtual_dirs.contains(internal) {
        return Ok(Some(dostat(a, None)));
    }
    for mounted in a.mounted_dirs.keys() {
        if mounted.len() >= internal.len() || !internal.starts_with(&mounted) {
            continue;
        }
        let Some(tmp) = archives(req)
            .get(id)
            .and_then(|a| a.manifest.get(&mounted))
            .filter(|e| e.is_mounted)
            .and_then(|e| e.tmp.clone())
        else {
            return Ok(None);
        };
        let mut test = tmp;
        test.extend_from_slice(&internal[mounted.len()..]);
        if req.url_stat(&test, false, false)?.found().is_none() {
            continue;
        }
        if !entry::mount(req, id, &test, internal)? {
            return Ok(None);
        }
        let Some(a) = archives(req).get(id) else {
            return Ok(None);
        };
        return Ok(a.manifest.get(internal).map(|e| dostat(a, Some(e))));
    }
    Ok(None)
}

/// `phar_make_dirstream`: the names directly under `dir`, each once,
/// sorted as `zend_binary_strcmp` sorts them; the magic `.phar` directory
/// and an empty archive's root list nothing.
fn make_dirstream(a: &archive::Archive, dir: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    if (dir == b"/" && a.manifest.is_empty()) || path::is_magic(dir) {
        return out;
    }
    for (key, _) in a.manifest.iter() {
        let keylen = key.len();
        if keylen <= dir.len() && (keylen == 0 || keylen < dir.len() || key.starts_with(dir)) {
            continue;
        }
        let name = if dir == b"/" {
            if path::is_magic(key) {
                continue;
            }
            match key.iter().position(|b| *b == b'/') {
                Some(slash) => &key[..slash],
                None => key,
            }
        } else {
            if !key.starts_with(dir) || key.get(dir.len()) != Some(&b'/') {
                continue;
            }
            let rest = &key[dir.len() + 1..];
            match rest.iter().position(|b| *b == b'/') {
                Some(slash) => &rest[..slash],
                None => rest,
            }
        };
        if !name.is_empty() && !out.iter().any(|n| n.as_slice() == name) {
            out.push(name.to_vec());
        }
    }
    out.sort();
    // `phar_dir_read` answers no entry for a name `d_name[MAXPATHLEN]`
    // cannot hold with its NUL, so `readdir()` meets `false` there.
    if let Some(long) = out.iter().position(|n| n.len() >= MAXPATHLEN) {
        out.truncate(long);
    }
    out
}

/// `phar_wrapper_open_dir`: the directory's entries, or a mounted
/// directory's own listing.
fn open_dir(req: &mut Request, path: &[u8]) -> Result<Option<DirListing>, Thrown> {
    Ok(list_dir_url(req, path)?.map(DirListing::Names))
}

/// The entries `phar_wrapper_open_dir` lists at `path`.
fn list_dir_url(req: &mut Request, path: &[u8]) -> Result<Option<Vec<Vec<u8>>>, Thrown> {
    let Some(url) = parse_url(req, path, b"r", Report::Log)? else {
        req.wrapper_log_error(&format!("phar url \"{}\" is unknown", lossy(path)));
        return Ok(None);
    };
    let id = match archive::get_archive(req, &url.host, None) {
        Ok(id) => id,
        Err(error) => {
            req.wrapper_log_error(
                &error.unwrap_or_else(|| format!("phar file \"{}\" is unknown", lossy(&url.host))),
            );
            return Ok(None);
        }
    };
    let Some(a) = archives(req).get(id) else {
        return Ok(None);
    };
    if url.path == b"/" {
        return Ok(Some(make_dirstream(a, b"/")));
    }
    let internal = &url.path[1..];
    match a.manifest.get(internal) {
        Some(e) if !e.is_dir => Ok(None),
        Some(e) if e.is_mounted => {
            let Some(tmp) = e.tmp.clone() else {
                return Ok(None);
            };
            list_dir(req, &tmp)
        }
        Some(_) => Ok(Some(make_dirstream(a, internal))),
        None if a.manifest.iter().any(|(k, _)| k.starts_with(internal)) => {
            Ok(Some(make_dirstream(a, internal)))
        }
        None => Ok(None),
    }
}

/// `php_stream_opendir` of a mounted directory, read whole.
fn list_dir(req: &mut Request, path: &[u8]) -> Result<Option<Vec<Vec<u8>>>, Thrown> {
    let Some(dir) = req.open_dir(path)? else {
        return Ok(None);
    };
    let mut out = Vec::new();
    loop {
        let name = match dir.read_dir(req) {
            Ok(Some(name)) => name,
            Ok(None) => break,
            Err(thrown) => {
                dir.free(req);
                return Err(thrown);
            }
        };
        out.push(name);
    }
    dir.free(req);
    Ok(Some(out))
}

/// `php_stream_phar_wrapper`, labelled `phar`.
///
/// ```php
/// $p = __DIR__ . '/phar-wrapper-witness.phar';
/// $files = ['a.txt' => 'hello', 'sub/b.txt' => 'nested', 'sub/deep/c.txt' => 'c'];
/// $m = ''; $body = '';
/// foreach ($files as $n => $c) { $m .= pack('V', strlen($n)) . $n . pack('VVVVVV', strlen($c), 1700000000, strlen($c), crc32($c), 0x1b6, 0); $body .= $c; }
/// $m = pack('VnVV', count($files), 0x1110, 0x10000, 3) . 'wit' . pack('V', 0) . $m;
/// $data = "<?php __HALT_COMPILER(); ?>\r\n" . pack('V', strlen($m)) . $m . $body;
/// file_put_contents($p, $data . sha1($data, true) . pack('V', 2) . 'GBMB');
/// var_dump(file_get_contents("phar://$p/a.txt"), file_get_contents('phar://wit/sub/b.txt'));
/// $h = fopen('phar://wit/sub/deep/c.txt', 'r');
/// $meta = stream_get_meta_data($h);
/// var_dump($meta['wrapper_type'], $meta['stream_type'], fread($h, 10), feof($h), fseek($h, 0), fgetc($h), fseek($h, 5));
/// fclose($h);
/// var_dump(scandir('phar://wit/'), scandir('phar://wit/sub'), is_dir('phar://wit/sub'), is_file('phar://wit/a.txt'), filesize('phar://wit/a.txt'), filemtime('phar://wit/sub/b.txt'), file_exists('phar://wit/nope'));
/// var_dump(@file_get_contents('phar://wit/nope'), error_get_last()['message']);
/// unlink($p);
/// ```
/// ```expect
/// string(5) "hello"
/// string(6) "nested"
/// string(4) "phar"
/// string(11) "phar stream"
/// string(1) "c"
/// bool(true)
/// int(0)
/// string(1) "c"
/// int(-1)
/// array(2) {
///   [0]=>
///   string(5) "a.txt"
///   [1]=>
///   string(3) "sub"
/// }
/// array(2) {
///   [0]=>
///   string(5) "b.txt"
///   [1]=>
///   string(4) "deep"
/// }
/// bool(true)
/// bool(true)
/// int(5)
/// int(1700000000)
/// bool(false)
/// bool(false)
/// string(%d) "file_get_contents(phar://wit/nope): Failed to open stream: phar error: "nope" is not a file in phar "%s""
/// ```
pub(crate) fn wrapper() -> StreamWrapper {
    StreamWrapper {
        scheme: "phar".into(),
        label: "phar",
        is_url: false,
        open: Some(open_url),
        open_dir: Some(open_dir),
        stat: Some(url_stat),
        unlink: Some(write::unlink),
        rename: Some(write::rename),
        mkdir: Some(write::mkdir),
        rmdir: Some(write::rmdir),
        set_stat: None,
        stream_stat: None,
    }
}
